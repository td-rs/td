//! `td::BusinessWorkHours` (BusinessWorkHours.h/.cpp/.hpp, M.48).
//!
//! The weekly-opening-hours value: the WorkHoursInterval record, the
//! byte-exact store/parse (flags, the interval vector, the time-zone id),
//! the td_api/telegram_api conversions and the sanitize/combine machine
//! (`sanitize_work_hours` + `combine_work_hour_intervals`). The
//! time-zone-offset and utc-time-offset reads ride the [`BusinessEnv`].

use crate::business_manager::BusinessEnv;
use crate::store_parse::{self, Parser};
use crate::td_api;
use tdmtproto::telegram_api;

/// `BusinessWorkHours::WorkHoursInterval` (BusinessWorkHours.h:36-57).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkHoursInterval {
    start_minute: i32,
    end_minute: i32,
}

impl WorkHoursInterval {
    /// `WorkHoursInterval(int32, int32)` (BusinessWorkHours.h:43-44).
    pub const fn new(start_minute: i32, end_minute: i32) -> WorkHoursInterval {
        WorkHoursInterval {
            start_minute,
            end_minute,
        }
    }

    /// `get_business_opening_hours_interval_object`
    /// (BusinessWorkHours.cpp:18-20).
    pub fn get_business_opening_hours_interval_object(
        self,
    ) -> td_api::BusinessOpeningHoursInterval {
        td_api::BusinessOpeningHoursInterval {
            start_minute: self.start_minute,
            end_minute: self.end_minute,
        }
    }

    /// `get_input_business_weekly_open` (BusinessWorkHours.cpp:22-24).
    pub fn get_input_business_weekly_open(self) -> telegram_api::BusinessWeeklyOpen {
        telegram_api::BusinessWeeklyOpen {
            start_minute: self.start_minute,
            end_minute: self.end_minute,
        }
    }

    /// `store` (BusinessWorkHours.hpp:13-16).
    pub fn store(self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.start_minute);
        store_parse::store_i32(out, self.end_minute);
    }

    /// `parse` (BusinessWorkHours.hpp:17-20).
    pub fn parse(parser: &mut Parser) -> std::result::Result<WorkHoursInterval, &'static str> {
        Ok(WorkHoursInterval {
            start_minute: parser.i32_()?,
            end_minute: parser.i32_()?,
        })
    }
}

impl std::fmt::Display for WorkHoursInterval {
    /// `operator<<` (BusinessWorkHours.cpp:232-238).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}; {})", self.start_minute, self.end_minute)
    }
}

/// `td::BusinessWorkHours` (BusinessWorkHours.h:18-71).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BusinessWorkHours {
    work_hours: Vec<WorkHoursInterval>,
    time_zone_id: String,
}

/// The week length in minutes.
const WEEK_MINUTES: i32 = 7 * 24 * 60;

impl BusinessWorkHours {
    /// `BusinessWorkHours(businessWorkHours)` (BusinessWorkHours.cpp:26-35).
    pub fn from_telegram(work_hours: Option<telegram_api::BusinessWorkHours>) -> BusinessWorkHours {
        let mut result = BusinessWorkHours::default();
        if let Some(work_hours) = work_hours {
            result.work_hours = work_hours
                .weekly_open
                .into_iter()
                .map(|weekly_open| {
                    WorkHoursInterval::new(weekly_open.start_minute, weekly_open.end_minute)
                })
                .collect();
            result.sanitize_work_hours();
            result.time_zone_id = work_hours.timezone_id;
        }
        result
    }

    /// `BusinessWorkHours(businessOpeningHours)` (BusinessWorkHours.cpp:37-47).
    pub fn from_td_api(work_hours: Option<td_api::BusinessOpeningHours>) -> BusinessWorkHours {
        let mut result = BusinessWorkHours::default();
        if let Some(work_hours) = work_hours {
            result.work_hours = work_hours
                .opening_hours
                .into_iter()
                .map(|interval| WorkHoursInterval::new(interval.start_minute, interval.end_minute))
                .collect();
            result.sanitize_work_hours();
            result.time_zone_id = work_hours.time_zone_id;
        }
        result
    }

    /// `BusinessWorkHours::is_empty` (BusinessWorkHours.h:54-56).
    pub fn is_empty(&self) -> bool {
        self.work_hours.is_empty()
    }

    /// `get_business_opening_hours_object` (BusinessWorkHours.cpp:49-66) —
    /// the multi-day intervals are split at the day boundaries; `None` for
    /// the C++ nullptr.
    pub fn get_business_opening_hours_object(&self) -> Option<td_api::BusinessOpeningHours> {
        if self.is_empty() {
            return None;
        }
        let mut intervals = Vec::new();
        for &work_hour in &self.work_hours {
            let mut interval = work_hour;
            while interval.start_minute / (24 * 60) + 1 < interval.end_minute / (24 * 60) {
                let mut prefix = interval;
                prefix.end_minute = (interval.start_minute / (24 * 60) + 1) * 24 * 60;
                interval.start_minute = prefix.end_minute;
                intervals.push(prefix.get_business_opening_hours_interval_object());
            }
            intervals.push(interval.get_business_opening_hours_interval_object());
        }
        Some(td_api::BusinessOpeningHours {
            time_zone_id: self.time_zone_id.clone(),
            opening_hours: intervals,
        })
    }

    /// `get_local_business_opening_hours_object` (BusinessWorkHours.cpp:68-105)
    /// — the intervals shifted by the time-zone offset; `None` for the C++
    /// nullptr (empty or a bot).
    pub fn get_local_business_opening_hours_object(
        &self,
        env: &mut impl BusinessEnv,
    ) -> Option<td_api::BusinessOpeningHours> {
        if self.is_empty() || env.is_bot() {
            return None;
        }
        let offset = (env.get_time_zone_offset(&self.time_zone_id)
            - env.get_option_integer("utc_time_offset", 0) as i32)
            / 60;
        if offset == 0 {
            return self.get_business_opening_hours_object();
        }

        let mut local_work_hours = BusinessWorkHours::default();
        for interval in &self.work_hours {
            let mut start_minute = interval.start_minute - offset;
            let mut end_minute = interval.end_minute - offset;
            if start_minute < 0 {
                if end_minute <= 24 * 60 {
                    start_minute += WEEK_MINUTES;
                    end_minute += WEEK_MINUTES;
                } else {
                    local_work_hours.work_hours.push(WorkHoursInterval::new(
                        start_minute + WEEK_MINUTES,
                        WEEK_MINUTES,
                    ));
                    start_minute = 0;
                }
            } else if end_minute > 8 * 24 * 60 {
                if start_minute >= WEEK_MINUTES {
                    start_minute -= WEEK_MINUTES;
                    end_minute -= WEEK_MINUTES;
                } else {
                    local_work_hours
                        .work_hours
                        .push(WorkHoursInterval::new(0, end_minute - WEEK_MINUTES));
                    end_minute = WEEK_MINUTES;
                }
            }
            local_work_hours
                .work_hours
                .push(WorkHoursInterval::new(start_minute, end_minute));
        }
        local_work_hours.sanitize_work_hours();
        local_work_hours.get_business_opening_hours_object()
    }

    /// `get_input_business_work_hours` (BusinessWorkHours.cpp:107-114);
    /// `None` for the C++ nullptr.
    pub fn get_input_business_work_hours(&self) -> Option<telegram_api::BusinessWorkHours> {
        if self.is_empty() {
            return None;
        }
        Some(telegram_api::BusinessWorkHours {
            flags: 0,
            open_now: false,
            weekly_open: self
                .work_hours
                .iter()
                .map(|interval| interval.get_input_business_weekly_open())
                .collect(),
            timezone_id: self.time_zone_id.clone(),
        })
    }

    /// `get_next_open_close_in` (BusinessWorkHours.cpp:116-141).
    pub fn get_next_open_close_in(
        &self,
        env: &mut impl BusinessEnv,
        unix_time: i32,
        is_close: bool,
    ) -> i32 {
        if self.is_empty() {
            return 0;
        }
        let get_week_time = |time: i32| {
            const WEEK_LENGTH: i32 = 7 * 86400;
            ((time % WEEK_LENGTH) + WEEK_LENGTH) % WEEK_LENGTH
        };
        // the Unix time 0 was on a Thursday, the first Monday was at 4 * 86400
        let current_week_time = get_week_time(unix_time - 4 * 86400);
        let offset = env.get_time_zone_offset(&self.time_zone_id);
        let mut result = 1_000_000_000;
        for interval in &self.work_hours {
            let change_week_time = get_week_time(
                (if is_close {
                    interval.end_minute
                } else {
                    interval.start_minute
                }) * 60
                    - offset,
            );
            let mut wait_time = change_week_time - current_week_time;
            if wait_time < 0 {
                wait_time += 7 * 86400;
            }
            if wait_time < result {
                result = wait_time;
            }
        }
        result
    }

    /// `sanitize_work_hours` (BusinessWorkHours.cpp:143-155).
    fn sanitize_work_hours(&mut self) {
        self.work_hours.retain(|interval| {
            let valid = interval.start_minute < interval.end_minute
                && interval.start_minute >= 0
                && interval.end_minute <= 8 * 24 * 60;
            if !valid {
                tdutils::log_info!("Ignore interval {interval}");
            }
            valid
        });
        self.combine_work_hour_intervals();
    }

    /// `combine_work_hour_intervals` (BusinessWorkHours.cpp:157-226) — the
    /// sort/merge/normalize machine, ported step-for-step.
    fn combine_work_hour_intervals(&mut self) {
        if self.work_hours.is_empty() {
            return;
        }

        self.work_hours
            .sort_by_key(|interval| interval.start_minute);

        let mut j = 0;
        for i in 1..self.work_hours.len() {
            debug_assert!(self.work_hours[i].start_minute >= self.work_hours[j].start_minute);
            if self.work_hours[i].start_minute <= self.work_hours[j].end_minute {
                self.work_hours[j].end_minute = self.work_hours[j]
                    .end_minute
                    .max(self.work_hours[i].end_minute);
            } else {
                j += 1;
                self.work_hours[j] = self.work_hours[i];
            }
        }
        self.work_hours.truncate(j + 1);

        // there must be no intervals longer than 1 week
        for interval in &mut self.work_hours {
            interval.end_minute = interval
                .end_minute
                .min(interval.start_minute + WEEK_MINUTES);
        }

        debug_assert!(!self.work_hours.is_empty());

        // if the last interval can be exactly merged with the first one, merge them
        if self.work_hours[0].start_minute != 0
            && self.work_hours[0].start_minute + WEEK_MINUTES
                == self.work_hours.last().unwrap().end_minute
        {
            if self.work_hours.last().unwrap().start_minute >= WEEK_MINUTES {
                self.work_hours[0].start_minute =
                    self.work_hours.last().unwrap().start_minute - WEEK_MINUTES;
                self.work_hours.pop();
                debug_assert!(!self.work_hours.is_empty());
            } else {
                self.work_hours[0].start_minute = 0;
                self.work_hours.last_mut().unwrap().end_minute = WEEK_MINUTES;
            }
        }

        // if there are intervals that intersect the first interval or start after the end of
        // the week, then they must be normalized
        let max_minute = self.work_hours[0].start_minute + WEEK_MINUTES;
        if self.work_hours.last().unwrap().end_minute > max_minute
            || self.work_hours.last().unwrap().start_minute >= WEEK_MINUTES
        {
            let size = self.work_hours.len();
            for i in 0..size {
                if self.work_hours[i].start_minute >= WEEK_MINUTES {
                    self.work_hours[i].start_minute -= WEEK_MINUTES;
                    self.work_hours[i].end_minute -= WEEK_MINUTES;
                } else if self.work_hours[i].end_minute > max_minute {
                    self.work_hours.push(WorkHoursInterval::new(
                        max_minute - WEEK_MINUTES,
                        self.work_hours[i].end_minute - WEEK_MINUTES,
                    ));
                    self.work_hours[i].end_minute = max_minute;
                }
            }
            tdutils::log_info!("Need to normalize {self:?}");
            self.combine_work_hour_intervals();
        }
    }

    /// `store` (BusinessWorkHours.hpp:21-27).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags(out, []);
        store_parse::store_i32(out, self.work_hours.len() as i32);
        for interval in &self.work_hours {
            interval.store(out);
        }
        store_parse::store_string(out, &self.time_zone_id);
    }

    /// `parse` (BusinessWorkHours.hpp:28-34).
    pub fn parse(parser: &mut Parser) -> std::result::Result<BusinessWorkHours, &'static str> {
        parser.parse_flags::<0>()?;
        let len = parser.i32_()?;
        let mut work_hours = Vec::with_capacity(len as usize);
        for _ in 0..len {
            work_hours.push(WorkHoursInterval::parse(parser)?);
        }
        Ok(BusinessWorkHours {
            work_hours,
            time_zone_id: parser.string()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_store_parse_round_trip() {
        let interval = WorkHoursInterval::new(9 * 60, 18 * 60);
        let mut out = Vec::new();
        interval.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(WorkHoursInterval::parse(&mut parser).unwrap(), interval);
    }

    #[test]
    fn empty_work_hours_answer_none() {
        let work_hours = BusinessWorkHours::default();
        assert!(work_hours.is_empty());
        assert_eq!(work_hours.get_business_opening_hours_object(), None);
        assert_eq!(work_hours.get_input_business_work_hours(), None);
    }

    #[test]
    fn sanitize_drops_invalid_and_merges_intersecting_intervals() {
        // BusinessWorkHours.cpp:143-155 — the invalid intervals are
        // dropped and the intersecting ones combined.
        let mut work_hours = BusinessWorkHours {
            work_hours: vec![
                WorkHoursInterval::new(9 * 60, 12 * 60),
                WorkHoursInterval::new(11 * 60, 13 * 60),
                WorkHoursInterval::new(-1, 60),
                WorkHoursInterval::new(20 * 60, 8 * 24 * 60 + 1),
            ],
            time_zone_id: "Europe/Moscow".to_owned(),
        };
        work_hours.sanitize_work_hours();
        assert_eq!(
            work_hours.work_hours,
            vec![WorkHoursInterval::new(9 * 60, 13 * 60)]
        );
    }

    #[test]
    fn object_splits_multi_day_intervals_at_day_boundaries() {
        // BusinessWorkHours.cpp:49-66 — an interval spanning two full day
        // boundaries (23:00 to 50:00) splits at each one.
        let work_hours = BusinessWorkHours {
            work_hours: vec![WorkHoursInterval::new(23 * 60, 50 * 60)],
            time_zone_id: "UTC".to_owned(),
        };
        let object = work_hours.get_business_opening_hours_object().unwrap();
        assert_eq!(object.opening_hours.len(), 2);
        assert_eq!(object.opening_hours[0].start_minute, 23 * 60);
        assert_eq!(object.opening_hours[0].end_minute, 24 * 60);
        assert_eq!(object.opening_hours[1].start_minute, 24 * 60);
        assert_eq!(object.opening_hours[1].end_minute, 50 * 60);
    }

    #[test]
    fn store_parse_round_trip() {
        let value = BusinessWorkHours {
            work_hours: vec![
                WorkHoursInterval::new(9 * 60, 18 * 60),
                WorkHoursInterval::new(23 * 60, 25 * 60),
            ],
            time_zone_id: "Europe/Moscow".to_owned(),
        };
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(BusinessWorkHours::parse(&mut parser).unwrap(), value);
    }
}
