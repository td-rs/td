//! `td::BusinessAwayMessage` + `BusinessAwayMessageSchedule`
//! (BusinessAwayMessage.h/.cpp/.hpp + BusinessAwayMessageSchedule.h/.cpp/.hpp,
//! M.48).

use crate::business_manager::BusinessEnv;
use crate::business_recipients::BusinessRecipients;
use crate::files::QuickReplyShortcutId;
use crate::store_parse::{self, Parser};
use crate::td_api;
use tdmtproto::telegram_api;

/// `BusinessAwayMessageSchedule::Type` (BusinessAwayMessageSchedule.h:26).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ScheduleType {
    #[default]
    Always,
    OutsideOfWorkHours,
    Custom,
}

impl ScheduleType {
    /// `store` — the `enum class Type : int32` slot.
    fn store(self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self as i32);
    }

    /// `parse`.
    fn parse(parser: &mut Parser) -> std::result::Result<ScheduleType, &'static str> {
        Ok(match parser.i32_()? {
            0 => ScheduleType::Always,
            1 => ScheduleType::OutsideOfWorkHours,
            2 => ScheduleType::Custom,
            _ => return Err("invalid BusinessAwayMessageSchedule type"),
        })
    }
}

/// `td::BusinessAwayMessageSchedule` (BusinessAwayMessageSchedule.h:14-48).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BusinessAwayMessageSchedule {
    type_: ScheduleType,
    start_date: i32,
    end_date: i32,
}

impl BusinessAwayMessageSchedule {
    /// `BusinessAwayMessageSchedule(BusinessAwayMessageSchedule)`
    /// (BusinessAwayMessageSchedule.cpp:12-29).
    pub fn from_telegram(
        schedule: telegram_api::BusinessAwayMessageSchedule,
    ) -> BusinessAwayMessageSchedule {
        match schedule {
            telegram_api::BusinessAwayMessageSchedule::businessAwayMessageScheduleAlways(_) => {
                BusinessAwayMessageSchedule::default()
            }
            telegram_api::BusinessAwayMessageSchedule::businessAwayMessageScheduleOutsideWorkHours(_) => {
                BusinessAwayMessageSchedule {
                    type_: ScheduleType::OutsideOfWorkHours,
                    ..BusinessAwayMessageSchedule::default()
                }
            }
            telegram_api::BusinessAwayMessageSchedule::businessAwayMessageScheduleCustom(custom) => {
                BusinessAwayMessageSchedule {
                    type_: ScheduleType::Custom,
                    start_date: custom.start_date,
                    end_date: custom.end_date,
                }
            }
        }
    }

    /// `BusinessAwayMessageSchedule(BusinessAwayMessageSchedule)`
    /// (BusinessAwayMessageSchedule.cpp:31-52) — `None` keeps the default
    /// Always like the C++ null-ctor early return.
    pub fn from_td_api(
        schedule: Option<td_api::BusinessAwayMessageSchedule>,
    ) -> BusinessAwayMessageSchedule {
        let Some(schedule) = schedule else {
            return BusinessAwayMessageSchedule::default();
        };
        match schedule {
            td_api::BusinessAwayMessageSchedule::businessAwayMessageScheduleAlways(_) => {
                BusinessAwayMessageSchedule::default()
            }
            td_api::BusinessAwayMessageSchedule::businessAwayMessageScheduleOutsideOfOpeningHours(_) => {
                BusinessAwayMessageSchedule {
                    type_: ScheduleType::OutsideOfWorkHours,
                    ..BusinessAwayMessageSchedule::default()
                }
            }
            td_api::BusinessAwayMessageSchedule::businessAwayMessageScheduleCustom(custom) => {
                BusinessAwayMessageSchedule {
                    type_: ScheduleType::Custom,
                    start_date: custom.start_date,
                    end_date: custom.end_date,
                }
            }
        }
    }

    /// `get_business_away_message_schedule_object`
    /// (BusinessAwayMessageSchedule.cpp:54-67).
    pub fn get_business_away_message_schedule_object(self) -> td_api::BusinessAwayMessageSchedule {
        match self.type_ {
            ScheduleType::Always => td_api::BusinessAwayMessageSchedule::businessAwayMessageScheduleAlways(
                td_api::businessAwayMessageScheduleAlways,
            ),
            ScheduleType::OutsideOfWorkHours => {
                td_api::BusinessAwayMessageSchedule::businessAwayMessageScheduleOutsideOfOpeningHours(
                    td_api::businessAwayMessageScheduleOutsideOfOpeningHours,
                )
            }
            ScheduleType::Custom => td_api::BusinessAwayMessageSchedule::businessAwayMessageScheduleCustom(
                td_api::businessAwayMessageScheduleCustom {
                    start_date: self.start_date,
                    end_date: self.end_date,
                },
            ),
        }
    }

    /// `get_input_business_away_message_schedule`
    /// (BusinessAwayMessageSchedule.cpp:69-80).
    pub fn get_input_business_away_message_schedule(
        self,
    ) -> telegram_api::BusinessAwayMessageSchedule {
        match self.type_ {
            ScheduleType::Always => telegram_api::BusinessAwayMessageSchedule::businessAwayMessageScheduleAlways(
                telegram_api::businessAwayMessageScheduleAlways,
            ),
            ScheduleType::OutsideOfWorkHours => {
                telegram_api::BusinessAwayMessageSchedule::businessAwayMessageScheduleOutsideWorkHours(
                    telegram_api::businessAwayMessageScheduleOutsideWorkHours,
                )
            }
            ScheduleType::Custom => telegram_api::BusinessAwayMessageSchedule::businessAwayMessageScheduleCustom(
                telegram_api::businessAwayMessageScheduleCustom {
                    start_date: self.start_date,
                    end_date: self.end_date,
                },
            ),
        }
    }

    /// `store` (BusinessAwayMessageSchedule.hpp:13-27).
    pub fn store(self, out: &mut Vec<u8>) {
        let has_start_date = self.start_date != 0;
        let has_end_date = self.end_date != 0;
        store_parse::store_flags(out, [has_start_date, has_end_date]);
        self.type_.store(out);
        if has_start_date {
            store_parse::store_i32(out, self.start_date);
        }
        if has_end_date {
            store_parse::store_i32(out, self.end_date);
        }
    }

    /// `parse` (BusinessAwayMessageSchedule.hpp:28-43).
    pub fn parse(
        parser: &mut Parser,
    ) -> std::result::Result<BusinessAwayMessageSchedule, &'static str> {
        let [has_start_date, has_end_date] = parser.parse_flags()?;
        let mut result = BusinessAwayMessageSchedule {
            type_: ScheduleType::parse(parser)?,
            ..BusinessAwayMessageSchedule::default()
        };
        if has_start_date {
            result.start_date = parser.i32_()?;
        }
        if has_end_date {
            result.end_date = parser.i32_()?;
        }
        Ok(result)
    }
}

impl std::fmt::Display for BusinessAwayMessageSchedule {
    /// `operator<<` (BusinessAwayMessageSchedule.cpp:82-100).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.type_ {
            ScheduleType::Always => write!(f, "always"),
            ScheduleType::OutsideOfWorkHours => write!(f, "outside of work hours"),
            ScheduleType::Custom => write!(f, "from {} to {}", self.start_date, self.end_date),
        }
    }
}

/// `td::BusinessAwayMessage` (BusinessAwayMessage.h:16-52).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BusinessAwayMessage {
    shortcut_id: QuickReplyShortcutId,
    recipients: BusinessRecipients,
    schedule: BusinessAwayMessageSchedule,
    offline_only: bool,
}

impl BusinessAwayMessage {
    /// `BusinessAwayMessage(businessAwayMessage)` (BusinessAwayMessage.cpp:10-16).
    pub fn from_telegram(
        away_message: Option<telegram_api::BusinessAwayMessage>,
    ) -> BusinessAwayMessage {
        let mut result = BusinessAwayMessage::default();
        let Some(away_message) = away_message else {
            return result;
        };
        result.shortcut_id = QuickReplyShortcutId::new(away_message.shortcut_id);
        result.recipients =
            BusinessRecipients::from_telegram(away_message.recipients.unwrap_or_default());
        result.schedule =
            BusinessAwayMessageSchedule::from_telegram(away_message.schedule.unwrap_or(
                telegram_api::BusinessAwayMessageSchedule::businessAwayMessageScheduleAlways(
                    telegram_api::businessAwayMessageScheduleAlways,
                ),
            ));
        result.offline_only = away_message.offline_only;
        result
    }

    /// `BusinessAwayMessage(businessAwayMessageSettings)`
    /// (BusinessAwayMessage.cpp:18-24).
    pub fn from_td_api(
        away_message: Option<td_api::BusinessAwayMessageSettings>,
    ) -> BusinessAwayMessage {
        let mut result = BusinessAwayMessage::default();
        let Some(away_message) = away_message else {
            return result;
        };
        result.shortcut_id = QuickReplyShortcutId::new(away_message.shortcut_id);
        result.recipients = BusinessRecipients::from_td_api(away_message.recipients, false);
        result.schedule = BusinessAwayMessageSchedule::from_td_api(away_message.schedule);
        result.offline_only = away_message.offline_only;
        result
    }

    /// `get_business_away_message_settings_object`
    /// (BusinessAwayMessage.cpp:26-32); `None` for the C++ nullptr.
    pub fn get_business_away_message_settings_object(
        &self,
        env: &mut impl BusinessEnv,
    ) -> Option<td_api::BusinessAwayMessageSettings> {
        if self.is_empty() {
            return None;
        }
        Some(td_api::BusinessAwayMessageSettings {
            shortcut_id: self.shortcut_id.get(),
            recipients: Some(self.recipients.get_business_recipients_object(env)),
            schedule: Some(self.schedule.get_business_away_message_schedule_object()),
            offline_only: self.offline_only,
        })
    }

    /// `get_input_business_away_message` (BusinessAwayMessage.cpp:34-38).
    pub fn get_input_business_away_message(
        &self,
        env: &mut impl BusinessEnv,
    ) -> telegram_api::InputBusinessAwayMessage {
        telegram_api::InputBusinessAwayMessage {
            flags: if self.offline_only { 1 } else { 0 },
            offline_only: self.offline_only,
            shortcut_id: self.shortcut_id.get(),
            schedule: Some(self.schedule.get_input_business_away_message_schedule()),
            recipients: Some(self.recipients.get_input_business_recipients(env)),
        }
    }

    /// `BusinessAwayMessage::is_empty` (BusinessAwayMessage.h:34-36).
    pub fn is_empty(&self) -> bool {
        !self.is_valid()
    }

    /// `BusinessAwayMessage::is_valid` (BusinessAwayMessage.h:38-40) —
    /// `QuickReplyShortcutId::is_server`, inlined.
    pub fn is_valid(&self) -> bool {
        self.shortcut_id.get() > 0
    }

    /// `add_dependencies` (BusinessAwayMessage.cpp:40-42).
    pub fn add_dependencies(&self, env: &mut impl BusinessEnv) {
        self.recipients.add_dependencies(env);
    }

    /// `store` (BusinessAwayMessage.hpp:13-20).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags(out, [self.offline_only]);
        store_parse::store_i32(out, self.shortcut_id.get());
        self.recipients.store(out);
        self.schedule.store(out);
    }

    /// `parse` (BusinessAwayMessage.hpp:21-28).
    pub fn parse(parser: &mut Parser) -> std::result::Result<BusinessAwayMessage, &'static str> {
        let [offline_only] = parser.parse_flags()?;
        Ok(BusinessAwayMessage {
            shortcut_id: QuickReplyShortcutId::new(parser.i32_()?),
            recipients: BusinessRecipients::parse(parser)?,
            schedule: BusinessAwayMessageSchedule::parse(parser)?,
            offline_only,
        })
    }
}

impl std::fmt::Display for BusinessAwayMessage {
    /// `operator<<` (BusinessAwayMessage.cpp:50-53).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "away message {} {} {}{}",
            self.shortcut_id.get(),
            self.recipients,
            self.schedule,
            if self.offline_only {
                " only offline"
            } else {
                ""
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_round_trip_covers_all_types() {
        for value in [
            BusinessAwayMessageSchedule::default(),
            BusinessAwayMessageSchedule {
                type_: ScheduleType::OutsideOfWorkHours,
                ..BusinessAwayMessageSchedule::default()
            },
            BusinessAwayMessageSchedule {
                type_: ScheduleType::Custom,
                start_date: 100,
                end_date: 200,
            },
        ] {
            let mut out = Vec::new();
            value.store(&mut out);
            let mut parser = Parser::new(&out);
            assert_eq!(
                BusinessAwayMessageSchedule::parse(&mut parser).unwrap(),
                value
            );
        }
    }

    #[test]
    fn schedule_td_api_conversions() {
        let custom = BusinessAwayMessageSchedule::from_td_api(Some(
            td_api::BusinessAwayMessageSchedule::businessAwayMessageScheduleCustom(
                td_api::businessAwayMessageScheduleCustom {
                    start_date: 5,
                    end_date: 9,
                },
            ),
        ));
        assert_eq!(
            custom.get_business_away_message_schedule_object(),
            td_api::BusinessAwayMessageSchedule::businessAwayMessageScheduleCustom(
                td_api::businessAwayMessageScheduleCustom {
                    start_date: 5,
                    end_date: 9
                }
            )
        );
        assert!(matches!(
            BusinessAwayMessageSchedule::from_td_api(None)
                .get_business_away_message_schedule_object(),
            td_api::BusinessAwayMessageSchedule::businessAwayMessageScheduleAlways(_)
        ));
    }

    #[test]
    fn away_message_round_trip() {
        let value = BusinessAwayMessage {
            shortcut_id: QuickReplyShortcutId::new(3),
            recipients: BusinessRecipients::default(),
            schedule: BusinessAwayMessageSchedule {
                type_: ScheduleType::Custom,
                start_date: 7,
                end_date: 8,
            },
            offline_only: true,
        };
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(BusinessAwayMessage::parse(&mut parser).unwrap(), value);
        assert!(value.is_valid());
        assert!(!BusinessAwayMessage::default().is_valid());
    }
}
