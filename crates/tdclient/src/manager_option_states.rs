//! Port of the four M.3.9.2 option-read fan-out receivers of
//! `ConfigManager::process_app_config` (ConfigManager.cpp:1976 and
//! 2050-2058): `TranscriptionManager::on_update_trial_parameters`,
//! `UserManager::on_update_freeze_state`,
//! `AccountManager::on_update_age_verification_parameters` and
//! `GroupCallManager::on_update_group_call_message_limits`.
//!
//! The C++ receivers live in their managers; the managers don't exist
//! yet, so the state rides the Td actor as [`ManagerOptionStates`] (the
//! inline-manager convention) and moves into each manager when that
//! manager lands, keeping the pmc codecs and the td update builders.
//! The app-config values arrive as [`ManagerUpdate`]s carried by
//! `ConfigActions`; the Td applies them with the binlog pmc and the
//! auth flags, and replays the persisted state at init
//! ([`ManagerOptionStates::load`], the manager constructors'
//! pmc reads in the C++ creation order).
//!
//! The `TranscriptionManager` half moved out with M.25
//! (`crate::transcription_manager`): the trial-parameters state, the
//! `speech_recognition_trial` pmc codec, the constructor restore and
//! the update builder all moved verbatim; the
//! [`ManagerUpdate::TrialParameters`] carrier stays for the
//! ConfigManager fan-out, and the Td routes it to the manager.
//!
//! Divergences (all documented inline):
//! - `updateAgeVerificationParameters` with null `parameters` (the C++
//!   sends it when verification stops being needed) cannot be
//!   expressed: the generated td_api models the typed object field as
//!   non-nullable. The update is skipped in that case.

use tddb::binlog_key_value::BinlogKeyValue;
use tdmtproto::telegram_api;

use crate::config_manager::{get_json_value_int, get_json_value_long, get_json_value_string};
use crate::td_api::{self, Object};

/// `td::AgeVerificationParameters` (AgeVerificationParameters.h) — the
/// account age-verification state persisted in the binlog pmc under
/// `age_verification` (AccountManager.cpp:843-845).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgeVerificationParameters {
    need_verification: bool,
    bot_username: String,
    country: String,
    min_age: i32,
}

impl AgeVerificationParameters {
    /// The C++ constructor (AgeVerificationParameters.cpp:15-32) — the
    /// invalid combinations are logged, the values are kept as received.
    pub fn new(
        need_verification: bool,
        bot_username: String,
        country: String,
        min_age: i32,
    ) -> AgeVerificationParameters {
        if need_verification {
            if bot_username.is_empty() || country.is_empty() || min_age <= 0 {
                tdutils::log_error!(
                    "Receive invalid age verification parameters: {min_age} {country} {bot_username}"
                );
            }
        } else if !bot_username.is_empty() || !country.is_empty() || min_age != 0 {
            tdutils::log_error!(
                "Receive unneeded age verification parameters: {min_age} {country} {bot_username}"
            );
        }
        AgeVerificationParameters {
            need_verification,
            bot_username,
            country,
            min_age,
        }
    }

    /// `AgeVerificationParameters::need_verification`.
    pub fn need_verification(&self) -> bool {
        self.need_verification
    }

    /// `AgeVerificationParameters::get_age_verification_parameters_object`
    /// (AgeVerificationParameters.cpp:34-40) — `nullptr` when no
    /// verification is needed.
    fn get_age_verification_parameters_object(&self) -> Option<td_api::AgeVerificationParameters> {
        if !self.need_verification {
            return None;
        }
        Some(td_api::AgeVerificationParameters {
            min_age: self.min_age,
            verification_bot_username: self.bot_username.clone(),
            country: self.country.clone(),
        })
    }

    /// `AgeVerificationParameters::store` (AgeVerificationParameters.hpp:18-25)
    /// — the log-event flags word plus the two strings and the int32.
    fn store(&self, out: &mut Vec<u8>) {
        let mut flags = 0i32;
        if self.need_verification {
            flags |= 1;
        }
        store_i32(out, flags);
        store_string(out, self.bot_username.as_bytes());
        store_string(out, self.country.as_bytes());
        store_i32(out, self.min_age);
    }

    /// `AgeVerificationParameters::parse` (AgeVerificationParameters.hpp:27-35).
    fn parse(input: &[u8]) -> Result<AgeVerificationParameters, &'static str> {
        let mut parser = Parser::new(input);
        let flags = parser.i32_()?;
        let bot_username = parser.string_()?;
        let country = parser.string_()?;
        let min_age = parser.i32_()?;
        parser.fetch_end()?;
        Ok(AgeVerificationParameters {
            need_verification: flags & 1 != 0,
            bot_username,
            country,
            min_age,
        })
    }
}

/// `td::GroupCallMessageLimit` (GroupCallMessageLimit.h) — one paid
/// group-call message tier parsed from the `stars_groupcall_message_limits`
/// app-config key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GroupCallMessageLimit {
    star_count: i64,
    pin_duration: i32,
    max_text_length: i32,
    max_emoji_count: i32,
    color1: i32,
    color2: i32,
    color_bg: i32,
}

impl GroupCallMessageLimit {
    /// The JSON constructor (GroupCallMessageLimit.cpp:16-57) — a
    /// non-object value logs and leaves the zero limit.
    fn from_json_value(limit: &telegram_api::JSONValue) -> GroupCallMessageLimit {
        let telegram_api::JSONValue::jsonObject(object) = limit else {
            tdutils::log_error!("Receive {limit:?}");
            return GroupCallMessageLimit::default();
        };
        let mut result = GroupCallMessageLimit::default();
        for value in &object.value {
            match value.key.as_str() {
                "stars" => {
                    result.star_count = get_star_count(get_json_value_long(
                        value.value.as_deref().unwrap(),
                        "stars",
                    ));
                }
                "pin_period" => {
                    result.pin_duration =
                        get_json_value_int(value.value.as_deref().unwrap(), "pin_period");
                }
                "text_length_max" => {
                    result.max_text_length =
                        get_json_value_int(value.value.as_deref().unwrap(), "text_length_max");
                }
                "emoji_max" => {
                    result.max_emoji_count =
                        get_json_value_int(value.value.as_deref().unwrap(), "emoji_max");
                }
                "color1" => {
                    result.color1 = get_color(value.value.as_deref().unwrap(), "color1");
                }
                "color2" => {
                    result.color2 = get_color(value.value.as_deref().unwrap(), "color2");
                }
                "color_bg" => {
                    result.color_bg = get_color(value.value.as_deref().unwrap(), "color_bg");
                }
                _ => {}
            }
        }
        result
    }

    /// `GroupCallMessageLimit::basic` (GroupCallMessageLimit.cpp:59-67).
    fn basic() -> GroupCallMessageLimit {
        GroupCallMessageLimit {
            max_text_length: 30,
            color1: 9788635,
            color2: 9788635,
            color_bg: 4786075,
            ..GroupCallMessageLimit::default()
        }
    }

    /// `GroupCallMessageLimit::is_valid` (GroupCallMessageLimit.cpp:69-72).
    fn is_valid(&self) -> bool {
        self.star_count >= 0
            && self.pin_duration >= 0
            && self.max_text_length > 0
            && self.max_emoji_count >= 0
            && is_valid_color(self.color1)
            && is_valid_color(self.color2)
            && is_valid_color(self.color_bg)
    }

    /// `GroupCallMessageLimit::get_group_call_message_level_object`
    /// (GroupCallMessageLimit.cpp:74-78).
    fn get_group_call_message_level_object(&self) -> td_api::GroupCallMessageLevel {
        td_api::GroupCallMessageLevel {
            min_star_count: self.star_count,
            pin_duration: self.pin_duration,
            max_text_length: self.max_text_length,
            max_custom_emoji_count: self.max_emoji_count,
            first_color: self.color1,
            second_color: self.color2,
            background_color: self.color_bg,
        }
    }

    /// `operator<` (GroupCallMessageLimit.cpp:88-91) — the levels are
    /// ordered by strictly decreasing star count.
    fn is_after(&self, next: &GroupCallMessageLimit) -> bool {
        self.star_count > next.star_count
    }

    /// `GroupCallMessageLimit::store` (GroupCallMessageLimit.hpp:21-31) —
    /// the (empty) log-event flags word plus the raw fields.
    fn store(&self, out: &mut Vec<u8>) {
        store_i32(out, 0);
        store_i64(out, self.star_count);
        store_i32(out, self.pin_duration);
        store_i32(out, self.max_text_length);
        store_i32(out, self.max_emoji_count);
        store_i32(out, self.color1);
        store_i32(out, self.color2);
        store_i32(out, self.color_bg);
    }

    /// `GroupCallMessageLimit::parse` (GroupCallMessageLimit.hpp:33-43).
    fn parse(parser: &mut Parser) -> Result<GroupCallMessageLimit, &'static str> {
        let _flags = parser.i32_()?;
        Ok(GroupCallMessageLimit {
            star_count: parser.i64_()?,
            pin_duration: parser.i32_()?,
            max_text_length: parser.i32_()?,
            max_emoji_count: parser.i32_()?,
            color1: parser.i32_()?,
            color2: parser.i32_()?,
            color_bg: parser.i32_()?,
        })
    }
}

/// `td::GroupCallMessageLimits` (GroupCallMessageLimit.h) — the tier
/// list persisted in the binlog pmc under `group_call_message_limits`
/// (GroupCallManager.cpp:7388).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GroupCallMessageLimits {
    limits: Vec<GroupCallMessageLimit>,
}

impl GroupCallMessageLimits {
    /// The JSON constructor (GroupCallMessageLimit.cpp:106-128) — `null`
    /// and non-array values leave the empty limits; a parsed array keeps
    /// only valid levels in strictly decreasing star order and appends
    /// the basic level when the tail is not a zero-star level.
    fn from_json_value(limits: &telegram_api::JSONValue) -> GroupCallMessageLimits {
        let telegram_api::JSONValue::jsonArray(array) = limits else {
            tdutils::log_error!("Receive {limits:?}");
            return GroupCallMessageLimits::default();
        };
        let mut result = GroupCallMessageLimits::default();
        for value in &array.value {
            let limit = GroupCallMessageLimit::from_json_value(value);
            if !limit.is_valid() {
                tdutils::log_error!("Receive an invalid group call message level");
                continue;
            }
            if !result.limits.is_empty() && !result.limits.last().expect("checked").is_after(&limit)
            {
                tdutils::log_error!("Receive limits in invalid order");
                continue;
            }
            result.limits.push(limit);
        }
        if result.limits.is_empty()
            || result
                .limits
                .last()
                .expect("checked")
                .is_after(&GroupCallMessageLimit::basic())
        {
            tdutils::log_error!("Receive no basic limit");
            result.limits.push(GroupCallMessageLimit::basic());
        }
        result
    }

    /// `GroupCallMessageLimits::basic` (GroupCallMessageLimit.cpp:130-134).
    fn basic() -> GroupCallMessageLimits {
        GroupCallMessageLimits {
            limits: vec![GroupCallMessageLimit::basic()],
        }
    }

    /// `GroupCallMessageLimits::get_update_group_call_message_levels_object`
    /// (GroupCallMessageLimit.cpp:146-149).
    fn get_update_group_call_message_levels_object(&self) -> td_api::Object {
        Object::updateGroupCallMessageLevels(td_api::updateGroupCallMessageLevels {
            levels: self
                .limits
                .iter()
                .map(GroupCallMessageLimit::get_group_call_message_level_object)
                .collect(),
        })
    }

    /// `GroupCallMessageLimits::store` (GroupCallMessageLimit.hpp:46-53) —
    /// the (empty) flags word plus the `td::store(vector)` int32 count
    /// and the elements.
    fn store(&self, out: &mut Vec<u8>) {
        store_i32(out, 0);
        store_i32(out, self.limits.len() as i32);
        for limit in &self.limits {
            limit.store(out);
        }
    }

    /// `GroupCallMessageLimits::parse` (GroupCallMessageLimit.hpp:55-62).
    fn parse(input: &[u8]) -> Result<GroupCallMessageLimits, &'static str> {
        let mut parser = Parser::new(input);
        let _flags = parser.i32_()?;
        let count = parser.i32_()?;
        // The C++ `vector` parse reads exactly `count` elements and
        // errors on a short buffer.
        let mut limits = Vec::new();
        for _ in 0..count {
            limits.push(GroupCallMessageLimit::parse(&mut parser)?);
        }
        parser.fetch_end()?;
        Ok(GroupCallMessageLimits { limits })
    }
}

/// The UserManager freeze fields (UserManager.h: `freeze_since_date_`,
/// `freeze_until_date_`, `freeze_appeal_url_`) persisted in the binlog
/// pmc under `freeze_state` (UserManager.cpp:4801-4807).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FreezeState {
    since_date: i32,
    until_date: i32,
    appeal_url: String,
}

impl FreezeState {
    /// The pmc form (UserManager.cpp:4802-4803) — the two dates and the
    /// url in one space-separated string; the load splits off the two
    /// leading numbers and keeps the rest as the url, so an url with
    /// spaces round-trips.
    fn to_pmc_string(&self) -> String {
        format!(
            "{} {} {}",
            self.since_date, self.until_date, self.appeal_url
        )
    }

    /// The `UserManager` constructor restore (UserManager.cpp:2773-2780).
    fn from_pmc_str(value: &str) -> FreezeState {
        let mut split = value.splitn(3, ' ');
        let since_date = to_integer_i32(split.next().unwrap_or(""));
        let until_date = to_integer_i32(split.next().unwrap_or(""));
        let appeal_url = split.next().unwrap_or("").to_string();
        FreezeState {
            since_date,
            until_date,
            appeal_url,
        }
    }

    /// `UserManager::get_update_freeze_state_object`
    /// (UserManager.cpp:4786-4789).
    fn get_update_freeze_state_object(&self) -> td_api::Object {
        Object::updateFreezeState(td_api::updateFreezeState {
            is_frozen: self.since_date > 0,
            freezing_date: self.since_date,
            deletion_date: self.until_date,
            appeal_link: self.appeal_url.clone(),
        })
    }
}

/// The remaining receiver states until their managers land — each
/// intake returns the td update the C++ manager sends, if any.
#[derive(Default)]
pub struct ManagerOptionStates {
    freeze_state: FreezeState,
    age_verification_parameters: AgeVerificationParameters,
    message_limits: GroupCallMessageLimits,
}

/// One `process_app_config` manager fan-out, carried by `ConfigActions`
/// for the Td to apply (the C++ `send_closure` arguments).
pub enum ManagerUpdate {
    /// `TranscriptionManager::on_update_trial_parameters`
    /// (ConfigManager.cpp:2050-2052) — routed to
    /// `crate::transcription_manager` by the Td since M.25.
    TrialParameters {
        weekly_number: i32,
        duration_max: i32,
        cooldown_until: i32,
    },
    /// `UserManager::on_update_freeze_state` (ConfigManager.cpp:2054-2055).
    FreezeState {
        since_date: i32,
        until_date: i32,
        appeal_url: String,
    },
    /// `AccountManager::on_update_age_verification_parameters`
    /// (ConfigManager.cpp:2057-2059).
    AgeVerificationParameters(AgeVerificationParameters),
    /// `GroupCallManager::on_update_group_call_message_limits`
    /// (ConfigManager.cpp:1976-1978) — the raw JSON value; the manager
    /// constructs the limits from it.
    GroupCallMessageLimits(Option<Box<telegram_api::JSONValue>>),
}

impl ManagerOptionStates {
    /// Apply one fan-out; `Some(update)` is what the C++ manager would
    /// have sent to the client. The trial fan-out is routed to the
    /// transcription manager by the Td before this runs.
    pub fn on_manager_update(
        &mut self,
        pmc: &mut BinlogKeyValue,
        update: ManagerUpdate,
    ) -> Option<td_api::Object> {
        match update {
            ManagerUpdate::TrialParameters { .. } => {
                unreachable!("the Td routes the trial fan-out to the transcription manager")
            }
            ManagerUpdate::FreezeState {
                since_date,
                until_date,
                appeal_url,
            } => self.on_freeze_state(pmc, since_date, until_date, appeal_url),
            ManagerUpdate::AgeVerificationParameters(parameters) => {
                self.on_age_verification_parameters(pmc, parameters)
            }
            ManagerUpdate::GroupCallMessageLimits(limits) => {
                self.on_group_call_message_limits(pmc, limits.as_deref())
            }
        }
    }

    /// `UserManager::on_update_freeze_state` (UserManager.cpp:4791-4807).
    fn on_freeze_state(
        &mut self,
        pmc: &mut BinlogKeyValue,
        since_date: i32,
        until_date: i32,
        appeal_url: String,
    ) -> Option<td_api::Object> {
        let new_state = FreezeState {
            since_date,
            until_date,
            appeal_url,
        };
        if new_state == self.freeze_state {
            return None;
        }
        self.freeze_state = new_state;
        let update = self.freeze_state.get_update_freeze_state_object();
        if self.freeze_state.since_date > 0 {
            pmc.set("freeze_state", &self.freeze_state.to_pmc_string());
        } else {
            pmc.erase("freeze_state");
        }
        Some(update)
    }

    /// `AccountManager::on_update_age_verification_parameters`
    /// (AccountManager.cpp:847-859).
    fn on_age_verification_parameters(
        &mut self,
        pmc: &mut BinlogKeyValue,
        parameters: AgeVerificationParameters,
    ) -> Option<td_api::Object> {
        if parameters == self.age_verification_parameters {
            return None;
        }
        self.age_verification_parameters = parameters;
        if self.age_verification_parameters.need_verification() {
            let mut blob = Vec::new();
            self.age_verification_parameters.store(&mut blob);
            pmc.set_raw(b"age_verification", &blob);
        } else {
            pmc.erase("age_verification");
        }
        self.send_update_age_verification_parameters()
    }

    /// `AccountManager::send_update_age_verification_parameters`
    /// (AccountManager.cpp:867-869) — `None` for the unrepresentable
    /// null-parameters update (the module docs).
    fn send_update_age_verification_parameters(&self) -> Option<td_api::Object> {
        self.age_verification_parameters
            .get_age_verification_parameters_object()
            .map(|parameters| {
                Object::updateAgeVerificationParameters(td_api::updateAgeVerificationParameters {
                    parameters: Some(parameters),
                })
            })
    }

    /// `GroupCallManager::on_update_group_call_message_limits`
    /// (GroupCallManager.cpp:7381-7389).
    fn on_group_call_message_limits(
        &mut self,
        pmc: &mut BinlogKeyValue,
        limits: Option<&telegram_api::JSONValue>,
    ) -> Option<td_api::Object> {
        // A null value (the C++ object_ptr) leaves the empty limits.
        let new_limits = match limits {
            Some(limits) => GroupCallMessageLimits::from_json_value(limits),
            None => GroupCallMessageLimits::default(),
        };
        if new_limits == self.message_limits {
            return None;
        }
        self.message_limits = new_limits;
        let update = self
            .message_limits
            .get_update_group_call_message_levels_object();
        let mut blob = Vec::new();
        self.message_limits.store(&mut blob);
        pmc.set_raw(b"group_call_message_limits", &blob);
        Some(update)
    }

    /// The manager constructors' pmc reads, in the C++ creation order
    /// (Td.cpp:1157 AccountManager, :1231 GroupCallManager, :1313
    /// TranscriptionManager — moved to `TranscriptionManager::init` with
    /// M.25, :1322 UserManager) — each restored state sends the same
    /// update the C++ constructor does.
    pub fn load(
        &mut self,
        pmc: &mut BinlogKeyValue,
        _is_authorized: bool,
        is_bot: bool,
        _unix_time: i32,
    ) -> Vec<td_api::Object> {
        let mut updates = Vec::new();
        // AccountManager start_up (AccountManager.cpp:823-830).
        {
            let blob = pmc.get_raw(b"age_verification");
            if !blob.is_empty() {
                self.age_verification_parameters =
                    AgeVerificationParameters::parse(&blob).expect("age_verification parse");
                assert!(
                    self.age_verification_parameters.need_verification(),
                    "CHECK(age_verification_parameters_.need_verification())"
                );
                updates.extend(self.send_update_age_verification_parameters());
            }
        }
        // GroupCallManager start_up (GroupCallManager.cpp:1983-1992) — a
        // corrupt or missing blob falls back to the basic level, and the
        // levels update is sent for every non-bot start.
        if !is_bot {
            match GroupCallMessageLimits::parse(&pmc.get_raw(b"group_call_message_limits")) {
                Ok(limits) => self.message_limits = limits,
                Err(_) => self.message_limits = GroupCallMessageLimits::basic(),
            }
            updates.push(
                self.message_limits
                    .get_update_group_call_message_levels_object(),
            );
        }
        // UserManager constructor (UserManager.cpp:2771-2787).
        if !is_bot {
            let value = pmc.get("freeze_state");
            if !value.is_empty() {
                self.freeze_state = FreezeState::from_pmc_str(&value);
                updates.push(self.freeze_state.get_update_freeze_state_object());
            }
        }
        updates
    }
}

/// `StarManager::get_star_count` (StarManager.cpp:1799-1818) — the
/// ±2^51 clamp with the error logs, without `allow_negative`.
fn get_star_count(amount: i64) -> i64 {
    let max_amount = 1i64 << 51;
    if amount < 0 {
        tdutils::log_error!("Receive Telegram Star amount = {amount}");
        return 0;
    }
    if amount > max_amount {
        tdutils::log_error!("Receive Telegram Star amount = {amount}");
        return max_amount;
    }
    amount
}

/// The `get_color` lambda (GroupCallMessageLimit.cpp:23-32) — the hex
/// string with the `> 0xFFFFFF` rejection.
fn get_color(json_value: &telegram_api::JSONValue, name: &str) -> i32 {
    let string = get_json_value_string(json_value, name);
    match hex_to_integer_safe_u32(&string) {
        Ok(color) if color <= 0xFFFFFF => color as i32,
        _ => {
            tdutils::log_error!("Receive {string} as {name}");
            0
        }
    }
}

/// `is_valid_color` (misc.cpp:430-432).
fn is_valid_color(color: i32) -> bool {
    (0..=0xFFFFFF).contains(&color)
}

/// `to_integer<int32>` (tdutils/td/utils/misc.h:156-182) — the leading
/// digits with the two's-complement clamping dance, never an error.
fn to_integer_i32(value: &str) -> i32 {
    let mut integer_value: u32 = 0;
    let mut chars = value.as_bytes().iter().copied();
    let mut is_negative = false;
    if let Some(b'-') = chars.clone().next() {
        is_negative = true;
        chars.next();
    }
    for byte in chars {
        if !byte.is_ascii_digit() {
            break;
        }
        integer_value = integer_value
            .wrapping_mul(10)
            .wrapping_add((byte - b'0') as u32);
    }
    if integer_value > i32::MAX as u32 {
        integer_value = integer_value.wrapping_neg();
        is_negative = !is_negative;
        if integer_value > i32::MAX as u32 {
            return i32::MIN;
        }
    }
    if is_negative {
        (integer_value as i32).wrapping_neg()
    } else {
        integer_value as i32
    }
}

/// `hex_to_integer_safe<uint32>` (tdutils/td/utils/misc.h:231-249).
fn hex_to_integer_safe_u32(value: &str) -> Result<u32, &'static str> {
    if value.is_empty() {
        return Err("String is empty");
    }
    let mut integer_value: u32 = 0;
    for byte in value.bytes() {
        let digit = match byte {
            b'0'..=b'9' => (byte - b'0') as u32,
            b'a'..=b'f' => (byte - b'a') as u32 + 10,
            b'A'..=b'F' => (byte - b'A') as u32 + 10,
            _ => return Err("String contains non-hex digit"),
        };
        if integer_value > u32::MAX / 16 {
            return Err("String hex number overflows");
        }
        integer_value = integer_value * 16 + digit;
    }
    Ok(integer_value)
}

/// A little-endian int32 at the parser position — `td::store(int32)` in
/// `tl_helpers.h`.
fn store_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// A little-endian int64 at the parser position — `td::store(int64)`.
fn store_i64(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// `td::store(Slice)` — the TL string length framing (1 byte under
/// 254, `254` + 3 LE bytes under 2^24, else `255` + 4 LE bytes + 3
/// zero bytes) followed by the raw bytes.
fn store_string(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = bytes.len();
    if len < 254 {
        out.push(len as u8);
    } else if len < (1 << 24) {
        out.push(254);
        out.extend_from_slice(&(len as u32).to_le_bytes()[..3]);
    } else {
        out.push(255);
        out.extend_from_slice(&(len as u32).to_le_bytes());
        out.extend_from_slice(&[0, 0, 0]);
    }
    out.extend_from_slice(bytes);
}

/// The shared-stream log-event parser over the pmc blobs — the reads
/// mirror `TlParser` (`fetch_int`, `fetch_long`, `fetch_string<Slice>`).
struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a [u8]) -> Parser<'a> {
        Parser { input, pos: 0 }
    }

    fn i32_(&mut self) -> Result<i32, &'static str> {
        if self.pos + 4 > self.input.len() {
            return Err("Invalid int");
        }
        let value = i32::from_le_bytes(
            self.input[self.pos..self.pos + 4]
                .try_into()
                .expect("length checked"),
        );
        self.pos += 4;
        Ok(value)
    }

    fn i64_(&mut self) -> Result<i64, &'static str> {
        if self.pos + 8 > self.input.len() {
            return Err("Invalid long");
        }
        let value = i64::from_le_bytes(
            self.input[self.pos..self.pos + 8]
                .try_into()
                .expect("length checked"),
        );
        self.pos += 8;
        Ok(value)
    }

    /// `parser.fetch_string<Slice>()` — the inverse of [`store_string`].
    fn string_(&mut self) -> Result<String, &'static str> {
        if self.pos >= self.input.len() {
            return Err("Invalid string");
        }
        let first = self.input[self.pos];
        self.pos += 1;
        let len = match first {
            0..=253 => first as usize,
            254 => {
                if self.pos + 3 > self.input.len() {
                    return Err("Invalid string");
                }
                let len = self.input[self.pos] as usize
                    | (self.input[self.pos + 1] as usize) << 8
                    | (self.input[self.pos + 2] as usize) << 16;
                self.pos += 3;
                len
            }
            _ => {
                if self.pos + 4 > self.input.len() {
                    return Err("Invalid string");
                }
                let len = u32::from_le_bytes(
                    self.input[self.pos..self.pos + 4]
                        .try_into()
                        .expect("length checked"),
                ) as usize;
                self.pos += 4;
                self.pos += 3; // the 3 zero bytes
                len
            }
        };
        if self.pos + len > self.input.len() {
            return Err("Invalid string");
        }
        let bytes = self.input[self.pos..self.pos + len].to_vec();
        self.pos += len;
        String::from_utf8(bytes).map_err(|_| "Invalid utf8 string")
    }

    /// `parser.fetch_end()` — the whole blob must be consumed.
    fn fetch_end(&self) -> Result<(), &'static str> {
        if self.pos != self.input.len() {
            return Err("Remaining data");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_object(fields: Vec<(&str, telegram_api::JSONValue)>) -> telegram_api::JSONValue {
        telegram_api::JSONValue::jsonObject(telegram_api::jsonObject {
            value: fields
                .into_iter()
                .map(|(key, value)| {
                    Box::new(telegram_api::JSONObjectValue {
                        key: key.to_string(),
                        value: Some(Box::new(value)),
                    })
                })
                .collect(),
        })
    }

    fn json_number(value: f64) -> telegram_api::JSONValue {
        telegram_api::JSONValue::jsonNumber(telegram_api::jsonNumber { value })
    }

    fn json_string(value: &str) -> telegram_api::JSONValue {
        telegram_api::JSONValue::jsonString(telegram_api::jsonString {
            value: value.to_string(),
        })
    }

    fn json_array(values: Vec<telegram_api::JSONValue>) -> telegram_api::JSONValue {
        telegram_api::JSONValue::jsonArray(telegram_api::jsonArray {
            value: values.into_iter().map(Box::new).collect(),
        })
    }

    /// The config_manager tests' file-backed binlog pmc stand-in.
    struct TempPmc {
        pmc: BinlogKeyValue,
        path: String,
    }

    impl TempPmc {
        fn new(name: &str) -> TempPmc {
            let path = std::env::temp_dir()
                .join(format!("td-rs-mos-{name}-{}.binlog", std::process::id()))
                .to_string_lossy()
                .into_owned();
            let _ = BinlogKeyValue::destroy(&path);
            let mut pmc = BinlogKeyValue::new();
            pmc.init(&path, tddb::db_key::DbKey::empty()).ensure();
            TempPmc { pmc, path }
        }
    }

    impl Drop for TempPmc {
        fn drop(&mut self) {
            self.pmc.close();
            let _ = BinlogKeyValue::destroy(&self.path);
        }
    }

    #[test]
    fn test_age_verification_parameters_round_trip() {
        for parameters in [
            AgeVerificationParameters::default(),
            AgeVerificationParameters::new(true, "bot".into(), "DE".into(), 18),
        ] {
            let mut blob = Vec::new();
            parameters.store(&mut blob);
            assert_eq!(AgeVerificationParameters::parse(&blob), Ok(parameters));
        }
        let object = AgeVerificationParameters::new(true, "bot".into(), "DE".into(), 18)
            .get_age_verification_parameters_object()
            .expect("needed");
        assert_eq!(object.min_age, 18);
        assert_eq!(object.verification_bot_username, "bot");
        assert_eq!(object.country, "DE");
        assert!(
            AgeVerificationParameters::default()
                .get_age_verification_parameters_object()
                .is_none()
        );
    }

    #[test]
    fn test_group_call_message_limits_round_trip() {
        let limits = GroupCallMessageLimits {
            limits: vec![
                GroupCallMessageLimit {
                    star_count: 1000,
                    pin_duration: 300,
                    max_text_length: 100,
                    max_emoji_count: 20,
                    color1: 0x95CD5B,
                    color2: 0x95CD5B,
                    color_bg: 0x49041B,
                },
                GroupCallMessageLimit::basic(),
            ],
        };
        let mut blob = Vec::new();
        limits.store(&mut blob);
        assert_eq!(GroupCallMessageLimits::parse(&blob), Ok(limits));
        // The empty form is the 8-byte flags+count prefix.
        let mut blob = Vec::new();
        GroupCallMessageLimits::default().store(&mut blob);
        assert_eq!(blob, vec![0; 8]);
    }

    #[test]
    fn test_group_call_message_limits_from_json() {
        // A valid two-level array with hex colors and star clamping.
        let limits = GroupCallMessageLimits::from_json_value(&json_array(vec![
            json_object(vec![
                ("stars", json_number(2251799813685248.0)),
                ("pin_period", json_number(3600.0)),
                ("text_length_max", json_number(100.0)),
                ("emoji_max", json_number(34.0)),
                ("color1", json_string("95CD5B")),
                ("color2", json_string("95cd5b")),
                ("color_bg", json_string("49041B")),
            ]),
            json_object(vec![]),
        ]));
        assert_eq!(limits.limits.len(), 2);
        assert_eq!(limits.limits[0].star_count, 1 << 51);
        assert_eq!(limits.limits[0].color1, 0x95CD5B);
        assert_eq!(limits.limits[0].color2, 0x95CD5B);
        assert_eq!(limits.limits[0].color_bg, 0x49041B);
        assert_eq!(limits.limits[1], GroupCallMessageLimit::basic());

        // An invalid level is dropped; a mis-ordered one is dropped; an
        // empty array gains the basic level.
        let limits = GroupCallMessageLimits::from_json_value(&json_array(vec![
            json_object(vec![("text_length_max", json_number(-1.0))]),
            json_object(vec![("stars", json_number(5.0))]),
            json_object(vec![("stars", json_number(10.0))]),
        ]));
        assert_eq!(limits.limits, vec![GroupCallMessageLimit::basic()]);

        // A zero-star tail replaces the basic fallback.
        let limits = GroupCallMessageLimits::from_json_value(&json_array(vec![json_object(vec![
            ("stars", json_number(0.0)),
            ("text_length_max", json_number(30.0)),
        ])]));
        assert_eq!(limits.limits.len(), 1);
        assert_eq!(limits.limits[0].star_count, 0);

        // A negative star count is invalid (get_star_count returns 0,
        // but the rest of the zero limit fails validity); a non-array
        // value leaves the empty limits.
        assert_eq!(
            GroupCallMessageLimits::from_json_value(&json_object(vec![])),
            GroupCallMessageLimits::default()
        );
    }

    #[test]
    fn test_freeze_state_pmc_round_trip() {
        let state = FreezeState {
            since_date: 123,
            until_date: 456,
            appeal_url: "https://example.org/a b".into(),
        };
        assert_eq!(state.to_pmc_string(), "123 456 https://example.org/a b");
        assert_eq!(FreezeState::from_pmc_str(&state.to_pmc_string()), state);
        // The C++ split keeps the remainder in the url.
        assert_eq!(
            FreezeState::from_pmc_str("7 8"),
            FreezeState {
                since_date: 7,
                until_date: 8,
                appeal_url: String::new(),
            }
        );
    }

    #[test]
    fn test_to_integer_i32() {
        assert_eq!(to_integer_i32("123"), 123);
        assert_eq!(to_integer_i32("-42abc"), -42);
        assert_eq!(to_integer_i32("abc"), 0);
        assert_eq!(to_integer_i32(""), 0);
        assert_eq!(to_integer_i32("2147483647"), i32::MAX);
        assert_eq!(to_integer_i32("-2147483648"), i32::MIN);
        assert_eq!(to_integer_i32("99999999999"), 1215752191);
    }

    #[test]
    fn test_hex_to_integer_safe_u32() {
        assert_eq!(hex_to_integer_safe_u32("95CD5B"), Ok(0x95CD5B));
        assert_eq!(hex_to_integer_safe_u32(""), Err("String is empty"));
        assert_eq!(
            hex_to_integer_safe_u32("95CD5G"),
            Err("String contains non-hex digit")
        );
        assert_eq!(
            hex_to_integer_safe_u32("100000000"),
            Err("String hex number overflows")
        );
    }

    #[test]
    fn test_on_freeze_state() {
        let mut pmc = TempPmc::new("states-2");
        let mut states = ManagerOptionStates::default();
        let update = states
            .on_freeze_state(&mut pmc.pmc, 100, 200, "https://example.org".into())
            .expect("changed");
        let Object::updateFreezeState(update) = update else {
            panic!("freeze state update");
        };
        assert!(update.is_frozen);
        assert_eq!(update.freezing_date, 100);
        assert_eq!(update.deletion_date, 200);
        assert_eq!(update.appeal_link, "https://example.org");
        assert_eq!(pmc.pmc.get("freeze_state"), "100 200 https://example.org");
        // Unchanged: no update.
        assert_eq!(
            states.on_freeze_state(&mut pmc.pmc, 100, 200, "https://example.org".into()),
            None
        );
        // A zero since date thaws and erases the pmc entry.
        let update = states
            .on_freeze_state(&mut pmc.pmc, 0, 200, String::new())
            .expect("changed");
        let Object::updateFreezeState(update) = update else {
            panic!("freeze state update");
        };
        assert!(!update.is_frozen);
        assert!(pmc.pmc.get("freeze_state").is_empty());
    }

    #[test]
    fn test_on_age_verification_parameters() {
        let mut pmc = TempPmc::new("states-3");
        let mut states = ManagerOptionStates::default();
        let parameters = AgeVerificationParameters::new(true, "bot".into(), "DE".into(), 18);
        let update = states
            .on_age_verification_parameters(&mut pmc.pmc, parameters.clone())
            .expect("changed");
        let Object::updateAgeVerificationParameters(update) = update else {
            panic!("age verification update");
        };
        assert_eq!(
            update
                .parameters
                .as_ref()
                .expect("age verification parameters")
                .min_age,
            18
        );
        assert!(pmc.pmc.get_raw(b"age_verification").len() > 4);
        // Unchanged: no update.
        assert_eq!(
            states.on_age_verification_parameters(&mut pmc.pmc, parameters),
            None
        );
        // Turning verification off erases the pmc entry; the C++
        // null-parameters update is unrepresentable (module docs).
        assert_eq!(
            states
                .on_age_verification_parameters(&mut pmc.pmc, AgeVerificationParameters::default()),
            None
        );
        assert!(pmc.pmc.get_raw(b"age_verification").is_empty());
    }

    #[test]
    fn test_on_group_call_message_limits() {
        let mut pmc = TempPmc::new("states-4");
        let mut states = ManagerOptionStates::default();
        // The basic fallback is NOT equal to the empty default state, so
        // even an empty array produces an update and a pmc write.
        let update = states
            .on_group_call_message_limits(&mut pmc.pmc, Some(&json_array(vec![])))
            .expect("changed");
        let Object::updateGroupCallMessageLevels(update) = update else {
            panic!("group call message levels update");
        };
        assert_eq!(update.levels.len(), 1);
        assert_eq!(update.levels[0].max_text_length, 30);
        assert!(pmc.pmc.get_raw(b"group_call_message_limits").len() > 8);
        // The same value again: no update.
        assert_eq!(
            states.on_group_call_message_limits(&mut pmc.pmc, Some(&json_array(vec![]))),
            None
        );
    }

    #[test]
    fn test_load() {
        let mut pmc = TempPmc::new("states-5");
        // A bot start reads nothing at all.
        let mut states = ManagerOptionStates::default();
        assert!(states.load(&mut pmc.pmc, true, true, 1000).is_empty());

        // An unauthorized non-bot start: only the group-call levels
        // update (the AccountManager read finds nothing).
        let mut states = ManagerOptionStates::default();
        let updates = states.load(&mut pmc.pmc, false, false, 1000);
        assert_eq!(updates.len(), 1);
        assert!(matches!(
            updates[0],
            Object::updateGroupCallMessageLevels(_)
        ));

        // A full restore in the C++ creation order: age verification,
        // group-call levels, freeze (the trial read moved to the
        // transcription manager with M.25).
        let mut pmc = TempPmc::new("states-6");
        let mut states = ManagerOptionStates::default();
        states.on_age_verification_parameters(
            &mut pmc.pmc,
            AgeVerificationParameters::new(true, "bot".into(), "DE".into(), 18),
        );
        states.on_group_call_message_limits(&mut pmc.pmc, Some(&json_array(vec![])));
        states.on_freeze_state(&mut pmc.pmc, 100, 200, "url".into());
        let mut restored = ManagerOptionStates::default();
        let updates = restored.load(&mut pmc.pmc, true, false, 2000);
        assert_eq!(updates.len(), 3);
        assert!(matches!(
            updates[0],
            Object::updateAgeVerificationParameters(_)
        ));
        assert!(matches!(
            updates[1],
            Object::updateGroupCallMessageLevels(_)
        ));
        assert!(matches!(updates[2], Object::updateFreezeState(_)));

        // A corrupt levels blob falls back to the basic level; a stale
        // trial blob in the pmc is the transcription manager's read.
        pmc.pmc.set_raw(b"group_call_message_limits", b"garbage");
        let mut restored = ManagerOptionStates::default();
        let updates = restored.load(&mut pmc.pmc, true, false, 1000);
        assert_eq!(updates.len(), 3);
        let Object::updateGroupCallMessageLevels(ref levels) = updates[1] else {
            panic!("group call message levels update");
        };
        assert_eq!(levels.levels.len(), 1);
    }

    #[test]
    #[should_panic(expected = "age_verification parse")]
    fn test_load_corrupt_age_verification_panics() {
        let mut pmc = TempPmc::new("states-7");
        pmc.pmc.set_raw(b"age_verification", b"garbage");
        let mut states = ManagerOptionStates::default();
        states.load(&mut pmc.pmc, true, false, 1000);
    }
}
