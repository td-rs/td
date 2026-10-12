//! Port of `td/utils/HttpDate.{h,cpp}` — RFC 1123-style `Date` header
//! parsing (`HttpDate::parse_http_date` → unix time).
//!
//! The C++ parser rides `td::Parser` for the token walk; the same minimal
//! token operations (`read_till`, `read_word`, `skip`, `skip_nofail`,
//! `skip_whitespaces` with the shared status) live in the private
//! `Parser` below. `to_integer_safe` maps to `str::parse` with the C++
//! failure behavior (an error surfaces through the parser status, or the
//! explicit checks in [`to_unix_time`]).

use crate::status::{Result, Status};
use crate::{try_result, try_status};

/// `HttpDate::is_leap`.
fn is_leap(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// `HttpDate::seconds_in_day`.
const SECONDS_IN_DAY: i32 = 24 * 60 * 60;

/// `HttpDate::days_in_month`.
pub fn days_in_month(year: i32, month: i32) -> i32 {
    const COUNT: [i32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    COUNT[(month - 1) as usize] + i32::from(month == 2 && is_leap(year))
}

/// `HttpDate::to_unix_time`.
pub fn to_unix_time(
    year: i32,
    month: i32,
    day: i32,
    hour: i32,
    minute: i32,
    second: i32,
) -> Result<i32> {
    if !(1970..=2037).contains(&year) {
        return Result::Err(Status::error_message("Invalid year"));
    }
    if !(1..=12).contains(&month) {
        return Result::Err(Status::error_message("Invalid month"));
    }
    if !(1..=days_in_month(year, month)).contains(&day) {
        return Result::Err(Status::error_message("Invalid day"));
    }
    if !(0..24).contains(&hour) {
        return Result::Err(Status::error_message("Invalid hour"));
    }
    if !(0..60).contains(&minute) {
        return Result::Err(Status::error_message("Invalid minute"));
    }
    if !(0..=60).contains(&second) {
        return Result::Err(Status::error_message("Invalid second"));
    }

    let mut res = 0;
    for y in 1970..year {
        res += (i32::from(is_leap(y)) + 365) * SECONDS_IN_DAY;
    }
    for m in 1..month {
        res += days_in_month(year, m) * SECONDS_IN_DAY;
    }
    res += (day - 1) * SECONDS_IN_DAY;
    res += hour * 60 * 60;
    res += minute * 60;
    res += second;
    Result::Ok(res)
}

/// The `td::Parser` subset `parse_http_date` uses: `read_till`, `read_word`,
/// `skip`, `skip_nofail`, `skip_whitespaces` and the sticky status.
struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
    status: Option<Status>,
}

impl<'a> Parser<'a> {
    fn new(data: &'a str) -> Self {
        Self {
            data: data.as_bytes(),
            pos: 0,
            status: None,
        }
    }

    fn fail(&mut self, message: &str) {
        if self.status.is_none() {
            self.status = Some(Status::error_message(message));
        }
    }

    /// `read_till_nofail(Slice)` — up to the first of the stop bytes.
    fn read_till_nofail(&mut self, stop: &[u8]) -> &'a str {
        let start = self.pos;
        while self.pos < self.data.len() && !stop.contains(&self.data[self.pos]) {
            self.pos += 1;
        }
        std::str::from_utf8(&self.data[start..self.pos]).unwrap_or_default()
    }

    /// `read_till(char)` — like the nofail variant, but errors when the
    /// stop byte is not the next character.
    fn read_till(&mut self, c: u8) -> &'a str {
        let res = self.read_till_nofail(&[c]);
        if self.pos == self.data.len() || self.data[self.pos] != c {
            self.fail(&format!("Read till '{}' failed", c as char));
            return "";
        }
        res
    }

    fn peek_char(&self) -> u8 {
        self.data.get(self.pos).copied().unwrap_or(0)
    }

    fn skip_nofail(&mut self, c: u8) {
        if self.peek_char() == c {
            self.pos += 1;
        }
    }

    fn skip(&mut self, c: u8) {
        if self.peek_char() != c {
            self.fail(&format!("Skip '{}' failed", c as char));
            return;
        }
        self.pos += 1;
    }

    fn skip_whitespaces(&mut self) {
        while matches!(self.peek_char(), b' ' | b'\t' | b'\r' | b'\n') {
            self.pos += 1;
        }
    }

    /// `read_word()` — `skip_whitespaces` + `read_till_nofail(" \t\r\n")`.
    fn read_word(&mut self) -> &'a str {
        self.skip_whitespaces();
        self.read_till_nofail(b" \t\r\n")
    }
}

fn to_integer_safe(value: &str) -> Result<i32> {
    match value.parse::<i32>() {
        Ok(v) => Result::Ok(v),
        Err(_) => Result::Err(Status::error_message("Can't parse integer")),
    }
}

/// The lower-case month names, `HttpDate::parse_http_date`.
const MONTH_NAMES: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// `HttpDate::parse_http_date` — the RFC 1123 `Wed, 15 Sep 2026 12:34:56 GMT`
/// shape; the weekday before the comma is ignored.
pub fn parse_http_date(slice: &str) -> Result<i32> {
    let mut p = Parser::new(slice);
    p.read_till(b','); // ignore week day
    p.skip(b',');
    p.skip_whitespaces();
    p.skip_nofail(b'0');
    try_result!(day, to_integer_safe(p.read_word()));
    let month_name = p.read_word().to_ascii_lowercase();
    try_result!(year, to_integer_safe(p.read_word()));
    p.skip_whitespaces();
    p.skip_nofail(b'0');
    let hour = p.read_till(b':');
    p.skip(b':');
    p.skip_nofail(b'0');
    let minute = p.read_till(b':');
    p.skip(b':');
    p.skip_nofail(b'0');
    let second = p.read_word();
    let gmt = p.read_word();
    try_status!(p.status.take().unwrap_or_else(Status::ok));

    try_result!(hour, to_integer_safe(hour));
    try_result!(minute, to_integer_safe(minute));
    try_result!(second, to_integer_safe(second));
    if gmt != "GMT" {
        return Result::Err(Status::error_message("Timezone must be GMT"));
    }

    let mut month = 0;
    for (m, name) in MONTH_NAMES.iter().enumerate() {
        if *name == month_name {
            month = m as i32 + 1;
            break;
        }
    }
    if month == 0 {
        return Result::Err(Status::error_message("Unknown month name"));
    }

    to_unix_time(year, month, day, hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unwrap(result: Result<i32>) -> i32 {
        match result {
            Result::Ok(v) => v,
            Result::Err(status) => panic!("expected a date, got {status}"),
        }
    }

    #[test]
    fn parses_rfc1123_dates() {
        // 2026-09-15 12:34:56 UTC (a recorded response `date` header value).
        assert_eq!(
            unwrap(parse_http_date("Tue, 15 Sep 2026 12:34:56 GMT")),
            1789475696
        );
        // The weekday word before the comma is ignored entirely and the
        // exact spacing does not matter.
        assert_eq!(
            unwrap(parse_http_date("Wed, 15 Sep 2026 12:34:56 GMT")),
            1789475696
        );
        assert_eq!(
            unwrap(parse_http_date("Tue,15 Sep 2026 12:34:56 GMT")),
            1789475696
        );
        // A leading zero is stripped (`skip_nofail('0')`).
        assert_eq!(
            unwrap(parse_http_date("Sat, 05 Sep 2026 00:00:01 GMT")),
            1788566401
        );
        // Leap years: 2024 and 2000 are ones; the epoch itself is 1970-01-01.
        assert_eq!(
            unwrap(parse_http_date("Thu, 29 Feb 2024 00:00:00 GMT")),
            1709164800
        );
        assert_eq!(unwrap(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT")), 0);
    }

    #[test]
    fn rejects_bad_dates() {
        assert!(parse_http_date("").is_error());
        assert!(parse_http_date("Tue 15 Sep 2026 12:34:56 GMT").is_error()); // no comma
        assert!(parse_http_date("Tue, 15 Sep 2026 12:34:56 UTC").is_error()); // Timezone must be GMT
        assert!(parse_http_date("Tue, 15 Qqq 2026 12:34:56 GMT").is_error()); // Unknown month name
        assert!(parse_http_date("Tue, 15 Sep 2026 12:34 GMT").is_error()); // no seconds
        assert!(parse_http_date("Tue, 15 Sep 2026 12:34:56").is_error()); // no timezone
        assert!(to_unix_time(1969, 1, 1, 0, 0, 0).is_error()); // Invalid year
        assert!(to_unix_time(2038, 1, 1, 0, 0, 0).is_error());
        assert!(to_unix_time(2026, 13, 1, 0, 0, 0).is_error()); // Invalid month
        assert!(to_unix_time(2026, 2, 29, 0, 0, 0).is_error()); // Invalid day (non-leap)
        assert!(to_unix_time(2024, 2, 29, 0, 0, 0).is_ok());
        assert!(to_unix_time(2026, 1, 1, 24, 0, 0).is_error()); // Invalid hour
        assert!(to_unix_time(2026, 1, 1, 0, 60, 0).is_error()); // Invalid minute
        assert!(to_unix_time(2026, 1, 1, 0, 0, 61).is_error()); // Invalid second
    }

    #[test]
    fn days_in_month_matches_c() {
        assert_eq!(days_in_month(2026, 9), 30);
        assert_eq!(days_in_month(2026, 2), 28);
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2000, 2), 29); // %400
        assert_eq!(days_in_month(1900, 2), 28); // %100 but not %400
    }
}
