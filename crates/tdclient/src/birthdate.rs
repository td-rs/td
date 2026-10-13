//! Port of `td/telegram/Birthdate.{h,cpp}` — the packed day/month/year
//! date attached to user profiles, with the byte-exact database
//! serialization (`store`/`parse` is the raw `int32`). The td_api object
//! conversion (`get_birthdate_object`) arrived with the Contacts step;
//! the telegram `birthday` intake builds through `Birthdate::new` (the
//! C++ `init`, which re-derives the year-0/absent rules).

use tdmtproto::telegram_api;

/// `td::Birthdate`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Birthdate {
    birthdate: i32,
}

impl Birthdate {
    /// `Birthdate::init` — the packing with the C++ validation rules.
    pub fn new(day: i32, month: i32, year: i32) -> Birthdate {
        let mut year = year;
        if !(1800..=3000).contains(&year) {
            year = 0;
        }
        if month <= 0 || month > 12 || day <= 0 || day > days_in_month(year, month) {
            return Birthdate::default();
        }
        Birthdate {
            birthdate: day | (month << 5) | (year << 9),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.birthdate == 0
    }

    pub fn get_day(&self) -> i32 {
        self.birthdate & 31
    }

    pub fn get_month(&self) -> i32 {
        (self.birthdate >> 5) & 15
    }

    pub fn get_year(&self) -> i32 {
        self.birthdate >> 9
    }

    /// `Birthdate::get_input_birthday` (Birthdate.cpp:44-51) — the wire
    /// form of the query sends (`users.suggestBirthday`,
    /// `account.updateBirthday`); the year rides only when present (the
    /// `birthday::YEAR_MASK` flag bit).
    pub fn get_input_birthday(&self) -> telegram_api::Birthday {
        /// `telegram_api::birthday::YEAR_MASK` — the generated code
        /// carries no constant; the flag bit is the year's presence.
        const YEAR_MASK: i32 = 1;
        let year = self.get_year();
        let flags = if year != 0 { YEAR_MASK } else { 0 };
        telegram_api::Birthday {
            flags,
            day: self.get_day(),
            month: self.get_month(),
            year,
        }
    }

    /// `Birthdate::store` — the raw packed `int32` (Birthdate.hpp).
    pub fn store(&self, out: &mut Vec<u8>) {
        crate::store_parse::store_i32(out, self.birthdate);
    }

    /// `Birthdate::parse`.
    pub fn parse(input: &[u8]) -> Result<Birthdate, &'static str> {
        let mut parser = crate::store_parse::Parser::new(input);
        let birthdate = Birthdate::parse_from(&mut parser)?;
        parser.fetch_end()?;
        Ok(birthdate)
    }

    /// The parse over a shared stream (a Birthdate stored inline inside
    /// the `UserFull` blob).
    pub fn parse_from(parser: &mut crate::store_parse::Parser) -> Result<Birthdate, &'static str> {
        Ok(Birthdate {
            birthdate: parser.i32_()?,
        })
    }

    /// `Birthdate::get_birthdate_object` (Birthdate.cpp:37-42) — the
    /// td_api form; `None` is the C++ `nullptr` for the empty date.
    pub fn get_birthdate_object(&self) -> Option<crate::td_api::Birthdate> {
        if self.is_empty() {
            return None;
        }
        Some(crate::td_api::Birthdate {
            day: self.get_day(),
            month: self.get_month(),
            year: self.get_year(),
        })
    }
}

/// `HttpDate::days_in_month` (tdutils/td/utils/HttpDate.h) — a fixed
/// table plus the Gregorian leap rule for February; year 0 counts as a
/// leap year exactly like the C++ (`is_leap(0)` is true).
fn days_in_month(year: i32, month: i32) -> i32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_and_unpacks() {
        let b = Birthdate::new(29, 2, 2000);
        assert_eq!((b.get_day(), b.get_month(), b.get_year()), (29, 2, 2000));
        assert!(!b.is_empty());
    }

    #[test]
    fn rejects_invalid_dates() {
        assert!(Birthdate::new(31, 4, 2000).is_empty());
        assert!(Birthdate::new(29, 2, 2001).is_empty());
        assert!(Birthdate::new(0, 1, 2000).is_empty());
        assert!(Birthdate::new(10, 13, 2000).is_empty());
        // An out-of-range year clamps to 0 (no year); a valid day/month
        // still makes the birthdate non-empty (`Birthdate::init`).
        let clamped = Birthdate::new(10, 1, 5000);
        assert!(!clamped.is_empty());
        assert_eq!(
            (clamped.get_day(), clamped.get_month(), clamped.get_year()),
            (10, 1, 0)
        );
    }

    #[test]
    fn serialization_is_the_raw_packed_int() {
        let b = Birthdate::new(7, 11, 1990);
        let mut bytes = Vec::new();
        b.store(&mut bytes);
        assert_eq!(bytes.len(), 4);
        assert_eq!(Birthdate::parse(&bytes).unwrap(), b);
    }
}
