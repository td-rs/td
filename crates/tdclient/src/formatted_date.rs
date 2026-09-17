//! Port of `td/telegram/FormattedDate.{h,cpp}`: the date+formatting-flags
//! pair carried by `messageEntityFormattedDate` / `textEntityTypeDateTime`
//! entities. The `date_flags_` bitset is the storage format of the C++
//! class (also its serialized form), so it is kept verbatim.

use crate::td_api;
use tdmtproto::telegram_api;
use tdutils::status::Status;

/// `FormattedDate::DateFlags`.
pub mod date_flags {
    pub const RELATIVE: i32 = 1;
    pub const SHORT_TIME: i32 = 2;
    pub const LONG_TIME: i32 = 4;
    pub const SHORT_DATE: i32 = 8;
    pub const LONG_DATE: i32 = 16;
    pub const DAY_OF_WEEK: i32 = 32;
}

/// `td::FormattedDate`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FormattedDate {
    date: i32,
    date_flags: i32,
}

impl FormattedDate {
    /// The full flag constructor (FormattedDate.cpp:19-44): a relative
    /// request swallows every other flag.
    pub fn from_flags(
        date: i32,
        relative: bool,
        short_time: bool,
        long_time: bool,
        short_date: bool,
        long_date: bool,
        day_of_week: bool,
    ) -> FormattedDate {
        if date <= 0 {
            tdutils::log_error!("Receive wrong date {date}");
        }
        let date_flags = if relative {
            date_flags::RELATIVE
        } else {
            let mut flags = 0;
            if short_time {
                flags |= date_flags::SHORT_TIME;
            } else if long_time {
                flags |= date_flags::LONG_TIME;
            }
            if short_date {
                flags |= date_flags::SHORT_DATE;
            } else if long_date {
                flags |= date_flags::LONG_DATE;
            }
            if day_of_week {
                flags |= date_flags::DAY_OF_WEEK;
            }
            flags
        };
        FormattedDate { date, date_flags }
    }

    /// The `(date, date_flags)` constructor, marked tests-only in C++.
    pub fn from_raw(date: i32, date_flags: i32) -> FormattedDate {
        FormattedDate { date, date_flags }
    }

    /// `FormattedDate::get_formatted_date` (FormattedDate.cpp:46-101).
    pub fn get_formatted_date(
        date: i32,
        type_: Option<&td_api::DateTimeFormattingType>,
    ) -> Result<FormattedDate, Status> {
        if date <= 0 {
            return Err(Status::error(400, "Invalid date specified"));
        }
        let mut date_flags = 0;
        if let Some(type_) = type_ {
            match type_ {
                td_api::DateTimeFormattingType::dateTimeFormattingTypeRelative(_) => {
                    date_flags = date_flags::RELATIVE;
                }
                td_api::DateTimeFormattingType::dateTimeFormattingTypeAbsolute(type_) => {
                    match &type_.time_precision {
                        td_api::DateTimePartPrecision::dateTimePartPrecisionNone(_) => {}
                        td_api::DateTimePartPrecision::dateTimePartPrecisionShort(_) => {
                            date_flags |= date_flags::SHORT_TIME;
                        }
                        td_api::DateTimePartPrecision::dateTimePartPrecisionLong(_) => {
                            date_flags |= date_flags::LONG_TIME;
                        }
                    }
                    match &type_.date_precision {
                        td_api::DateTimePartPrecision::dateTimePartPrecisionNone(_) => {}
                        td_api::DateTimePartPrecision::dateTimePartPrecisionShort(_) => {
                            date_flags |= date_flags::SHORT_DATE;
                        }
                        td_api::DateTimePartPrecision::dateTimePartPrecisionLong(_) => {
                            date_flags |= date_flags::LONG_DATE;
                        }
                    }
                    if type_.show_day_of_week {
                        date_flags |= date_flags::DAY_OF_WEEK;
                    }
                }
            }
        }
        Ok(FormattedDate { date, date_flags })
    }

    /// `FormattedDate::get_formatted_date(date, date_format)`
    /// (FormattedDate.cpp:103-109).
    pub fn get_formatted_date_by_format(
        date: i32,
        date_format: &str,
    ) -> Result<FormattedDate, Status> {
        let date_flags = get_date_flags(date_format)?;
        Ok(FormattedDate { date, date_flags })
    }

    pub fn is_valid(self) -> bool {
        self.date > 0
    }

    pub fn get_date(self) -> i32 {
        self.date
    }

    pub fn date_flags(self) -> i32 {
        self.date_flags
    }

    /// `FormattedDate::get_date_time_formatting_type_object`
    /// (FormattedDate.cpp:139-172): `None` mirrors the C++ null pointer,
    /// emitted when no formatting was requested.
    pub fn get_date_time_formatting_type_object(self) -> Option<td_api::DateTimeFormattingType> {
        use td_api::{
            DateTimeFormattingType, DateTimePartPrecision, dateTimeFormattingTypeAbsolute,
            dateTimeFormattingTypeRelative, dateTimePartPrecisionLong, dateTimePartPrecisionNone,
            dateTimePartPrecisionShort,
        };
        if self.date_flags == 0 {
            return None;
        }
        if (self.date_flags & date_flags::RELATIVE) != 0 {
            return Some(DateTimeFormattingType::dateTimeFormattingTypeRelative(
                dateTimeFormattingTypeRelative {},
            ));
        }
        let time_precision = if (self.date_flags & date_flags::SHORT_TIME) != 0 {
            DateTimePartPrecision::dateTimePartPrecisionShort(dateTimePartPrecisionShort {})
        } else if (self.date_flags & date_flags::LONG_TIME) != 0 {
            DateTimePartPrecision::dateTimePartPrecisionLong(dateTimePartPrecisionLong {})
        } else {
            DateTimePartPrecision::dateTimePartPrecisionNone(dateTimePartPrecisionNone {})
        };

        let date_precision = if (self.date_flags & date_flags::SHORT_DATE) != 0 {
            DateTimePartPrecision::dateTimePartPrecisionShort(dateTimePartPrecisionShort {})
        } else if (self.date_flags & date_flags::LONG_DATE) != 0 {
            DateTimePartPrecision::dateTimePartPrecisionLong(dateTimePartPrecisionLong {})
        } else {
            DateTimePartPrecision::dateTimePartPrecisionNone(dateTimePartPrecisionNone {})
        };
        let show_day_of_week = (self.date_flags & date_flags::DAY_OF_WEEK) != 0;
        Some(DateTimeFormattingType::dateTimeFormattingTypeAbsolute(
            dateTimeFormattingTypeAbsolute {
                time_precision,
                date_precision,
                show_day_of_week,
            },
        ))
    }

    /// `FormattedDate::get_input_message_entity_formatted_date`
    /// (FormattedDate.cpp:174-181).
    pub fn get_input_message_entity_formatted_date(
        self,
        offset: i32,
        length: i32,
    ) -> telegram_api::messageEntityFormattedDate {
        telegram_api::messageEntityFormattedDate {
            flags: 0,
            relative: (self.date_flags & date_flags::RELATIVE) != 0,
            short_time: (self.date_flags & date_flags::SHORT_TIME) != 0,
            long_time: (self.date_flags & date_flags::LONG_TIME) != 0,
            short_date: (self.date_flags & date_flags::SHORT_DATE) != 0,
            long_date: (self.date_flags & date_flags::LONG_DATE) != 0,
            day_of_week: (self.date_flags & date_flags::DAY_OF_WEEK) != 0,
            offset,
            length,
            date: self.date,
        }
    }

    /// `FormattedDate::get_input_text_date` (FormattedDate.cpp:183-189).
    pub fn get_input_text_date(self, text: Box<telegram_api::RichText>) -> telegram_api::textDate {
        telegram_api::textDate {
            flags: 0,
            relative: (self.date_flags & date_flags::RELATIVE) != 0,
            short_time: (self.date_flags & date_flags::SHORT_TIME) != 0,
            long_time: (self.date_flags & date_flags::LONG_TIME) != 0,
            short_date: (self.date_flags & date_flags::SHORT_DATE) != 0,
            long_date: (self.date_flags & date_flags::LONG_DATE) != 0,
            day_of_week: (self.date_flags & date_flags::DAY_OF_WEEK) != 0,
            text,
            date: self.date,
        }
    }

    /// `FormattedDate::store` (FormattedDate.hpp).
    pub fn store(&self, out: &mut Vec<u8>) {
        crate::store_parse::store_i32(out, self.date);
        crate::store_parse::store_i32(out, self.date_flags);
    }

    /// `FormattedDate::parse` (FormattedDate.hpp).
    pub fn parse(input: &mut crate::store_parse::Parser) -> Result<FormattedDate, &'static str> {
        let date = input.i32_()?;
        let date_flags = input.i32_()?;
        Ok(FormattedDate { date, date_flags })
    }
}

/// `FormattedDate::get_date_flags` (FormattedDate.cpp:111-137).
pub fn get_date_flags(date_format: &str) -> Result<i32, Status> {
    if date_format == "r" || date_format == "R" {
        return Ok(date_flags::RELATIVE);
    }
    let mut date_flags = 0;
    for c in date_format.chars() {
        match c {
            't' => date_flags |= date_flags::SHORT_TIME,
            'T' => date_flags |= date_flags::LONG_TIME,
            'd' => date_flags |= date_flags::SHORT_DATE,
            'D' => date_flags |= date_flags::LONG_DATE,
            'w' | 'W' => date_flags |= date_flags::DAY_OF_WEEK,
            _ => return Err(Status::error(400, "Invalid date format used")),
        }
    }
    Ok(date_flags)
}

impl std::fmt::Display for FormattedDate {
    /// `operator<<(StringBuilder, FormattedDate)` (FormattedDate.cpp:195).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "date {} with flags {}", self.date, self.date_flags)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn absolute(time: &str, date: &str, day_of_week: bool) -> td_api::DateTimeFormattingType {
        use td_api::*;
        let precision = |name: &str| match name {
            "s" => DateTimePartPrecision::dateTimePartPrecisionShort(dateTimePartPrecisionShort {}),
            "l" => DateTimePartPrecision::dateTimePartPrecisionLong(dateTimePartPrecisionLong {}),
            _ => DateTimePartPrecision::dateTimePartPrecisionNone(dateTimePartPrecisionNone {}),
        };
        DateTimeFormattingType::dateTimeFormattingTypeAbsolute(dateTimeFormattingTypeAbsolute {
            time_precision: precision(time),
            date_precision: precision(date),
            show_day_of_week: day_of_week,
        })
    }

    #[test]
    fn relative_flag_swallows_the_rest() {
        // FormattedDate.cpp:19-44 — a relative request clears the others.
        let d = FormattedDate::from_flags(100, true, true, true, true, true, true);
        assert_eq!(d.date_flags(), date_flags::RELATIVE);
        let d = FormattedDate::from_flags(100, false, true, false, false, true, true);
        assert_eq!(
            d.date_flags(),
            date_flags::SHORT_TIME | date_flags::LONG_DATE | date_flags::DAY_OF_WEEK
        );
    }

    #[test]
    fn get_formatted_date_by_type() {
        let d = FormattedDate::get_formatted_date(
            1726500000,
            Some(
                &td_api::DateTimeFormattingType::dateTimeFormattingTypeRelative(
                    td_api::dateTimeFormattingTypeRelative {},
                ),
            ),
        )
        .unwrap();
        assert_eq!(d.date_flags(), date_flags::RELATIVE);

        let d =
            FormattedDate::get_formatted_date(1726500000, Some(&absolute("s", "l", true))).unwrap();
        assert_eq!(
            d.date_flags(),
            date_flags::SHORT_TIME | date_flags::LONG_DATE | date_flags::DAY_OF_WEEK
        );

        // No formatting type at all — flags stay zero.
        let d = FormattedDate::get_formatted_date(1726500000, None).unwrap();
        assert_eq!(d.date_flags(), 0);

        assert_eq!(
            FormattedDate::get_formatted_date(0, None)
                .unwrap_err()
                .code(),
            400
        );
    }

    #[test]
    fn get_formatted_date_by_format_string() {
        assert_eq!(
            FormattedDate::get_formatted_date_by_format(5, "r")
                .unwrap()
                .date_flags(),
            date_flags::RELATIVE
        );
        assert_eq!(
            FormattedDate::get_formatted_date_by_format(5, "R")
                .unwrap()
                .date_flags(),
            date_flags::RELATIVE
        );
        assert_eq!(
            FormattedDate::get_formatted_date_by_format(5, "tDw")
                .unwrap()
                .date_flags(),
            date_flags::SHORT_TIME | date_flags::LONG_DATE | date_flags::DAY_OF_WEEK
        );
        assert_eq!(
            FormattedDate::get_formatted_date_by_format(5, "x")
                .unwrap_err()
                .message(),
            "Invalid date format used"
        );
    }

    #[test]
    fn formatting_type_object_round_trip() {
        // flags == 0 -> C++ null -> Rust None
        assert!(
            FormattedDate::from_raw(100, 0)
                .get_date_time_formatting_type_object()
                .is_none()
        );
        let d = FormattedDate::from_raw(100, date_flags::RELATIVE);
        match d.get_date_time_formatting_type_object() {
            Some(td_api::DateTimeFormattingType::dateTimeFormattingTypeRelative(_)) => {}
            other => panic!("wrong type {other:?}"),
        }
        // get_formatted_date(formatting_type) inverts the object builder.
        let d = FormattedDate::from_raw(
            100,
            date_flags::LONG_TIME | date_flags::SHORT_DATE | date_flags::DAY_OF_WEEK,
        );
        let obj = d.get_date_time_formatting_type_object().unwrap();
        let back = FormattedDate::get_formatted_date(100, Some(&obj)).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn input_objects_carry_the_flags() {
        let d = FormattedDate::from_raw(
            1726500000,
            date_flags::RELATIVE | date_flags::SHORT_TIME | date_flags::DAY_OF_WEEK,
        );
        let e = d.get_input_message_entity_formatted_date(7, 9);
        assert!(e.relative && e.short_time && e.day_of_week);
        assert!(!e.long_time && !e.short_date && !e.long_date);
        assert_eq!((e.offset, e.length, e.date), (7, 9, 1726500000));

        let t = d.get_input_text_date(Box::new(telegram_api_rich_text_plain()));
        assert!(t.relative && t.short_time && t.day_of_week);
        assert_eq!(t.date, 1726500000);
    }

    fn telegram_api_rich_text_plain() -> telegram_api::RichText {
        use tdmtproto::telegram_api;
        telegram_api::RichText::textPlain(telegram_api::textPlain {
            text: "hi".to_owned(),
        })
    }

    #[test]
    fn store_parse_round_trip() {
        let d = FormattedDate::from_raw(-7, 63);
        let mut out = Vec::new();
        d.store(&mut out);
        let mut parser = crate::store_parse::Parser::new(&out);
        let back = FormattedDate::parse(&mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(back, d);
        assert_eq!(back.to_string(), "date -7 with flags 63");
    }
}
