//! `td::StoryStealthMode` (StoryStealthMode.h/.hpp/.cpp, M.47).

use crate::store_parse::{self, Parser};
use crate::td_api;
use tdmtproto::telegram_api;

/// `td::StoryStealthMode` (StoryStealthMode.h:19-55).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoryStealthMode {
    active_until_date: i32,
    cooldown_until_date: i32,
}

impl StoryStealthMode {
    /// `StoryStealthMode(storiesStealthMode)` (StoryStealthMode.cpp:13-16) —
    /// runs the `update` expiry pass like the C++ constructor.
    pub fn from_telegram(
        stealth_mode: telegram_api::StoriesStealthMode,
        unix_time: i32,
    ) -> StoryStealthMode {
        let mut result = StoryStealthMode {
            active_until_date: stealth_mode.active_until_date,
            cooldown_until_date: stealth_mode.cooldown_until_date,
        };
        result.update(unix_time);
        result
    }

    /// `StoryStealthMode::is_empty` (StoryStealthMode.h:27-29).
    pub fn is_empty(&self) -> bool {
        self.active_until_date == 0 && self.cooldown_until_date == 0
    }

    /// `StoryStealthMode::update` (StoryStealthMode.cpp:18-29) — the
    /// `G()->unix_time()` read rides the env-provided clock.
    pub fn update(&mut self, unix_time: i32) -> bool {
        let mut result = false;
        if self.active_until_date != 0 && self.active_until_date <= unix_time {
            self.active_until_date = 0;
            result = true;
        }
        if self.cooldown_until_date != 0 && self.cooldown_until_date <= unix_time {
            self.cooldown_until_date = 0;
            result = true;
        }
        result
    }

    /// `StoryStealthMode::get_update_date` (StoryStealthMode.cpp:31-42).
    pub fn get_update_date(&self) -> i32 {
        if self.active_until_date > 0 {
            if self.cooldown_until_date > 0 {
                return self.active_until_date.min(self.cooldown_until_date);
            }
            return self.active_until_date;
        }
        if self.cooldown_until_date > 0 {
            return self.cooldown_until_date;
        }
        0
    }

    /// `StoryStealthMode::get_update_story_stealth_mode_object`
    /// (StoryStealthMode.cpp:44-47).
    pub fn get_update_story_stealth_mode_object(&self) -> td_api::Update {
        td_api::Update::updateStoryStealthMode(td_api::updateStoryStealthMode {
            active_until_date: self.active_until_date,
            cooldown_until_date: self.cooldown_until_date,
        })
    }

    /// `StoryStealthMode::store` (StoryStealthMode.hpp:14-32).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_active_until_date = self.active_until_date != 0;
        let has_cooldown_until_date = self.cooldown_until_date != 0;
        store_parse::store_flags(out, [has_active_until_date, has_cooldown_until_date]);
        if has_active_until_date {
            store_parse::store_i32(out, self.active_until_date);
        }
        if has_cooldown_until_date {
            store_parse::store_i32(out, self.cooldown_until_date);
        }
    }

    /// `StoryStealthMode::parse` (StoryStealthMode.hpp:34-51).
    pub fn parse(parser: &mut Parser) -> std::result::Result<StoryStealthMode, &'static str> {
        let [has_active_until_date, has_cooldown_until_date] = parser.parse_flags()?;
        let mut result = StoryStealthMode::default();
        if has_active_until_date {
            result.active_until_date = parser.i32_()?;
        }
        if has_cooldown_until_date {
            result.cooldown_until_date = parser.i32_()?;
        }
        Ok(result)
    }
}

impl std::fmt::Display for StoryStealthMode {
    /// `operator<<(StringBuilder, StoryStealthMode)` (StoryStealthMode.cpp:50-62).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.active_until_date != 0 {
            return write!(f, "Stealth mode is active until {}", self.active_until_date);
        }
        if self.cooldown_until_date != 0 {
            return write!(
                f,
                "Stealth mode can't be activated until {}",
                self.cooldown_until_date
            );
        }
        write!(f, "Stealth mode can be activated")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_expires_passed_dates() {
        // StoryStealthMode.cpp:18-29 — the expiry pass zeroes each date
        // that fell at or before the clock.
        let mut mode = StoryStealthMode {
            active_until_date: 100,
            cooldown_until_date: 200,
        };
        assert!(!mode.update(99));
        assert_eq!(mode.active_until_date, 100);
        assert!(mode.update(100));
        assert_eq!(mode.active_until_date, 0);
        assert_eq!(mode.cooldown_until_date, 200);
        assert!(mode.update(300));
        assert!(mode.is_empty());
        assert!(!mode.update(300));
    }

    #[test]
    fn get_update_date_picks_the_earliest() {
        let mode = StoryStealthMode {
            active_until_date: 300,
            cooldown_until_date: 100,
        };
        assert_eq!(mode.get_update_date(), 100);
        assert_eq!(
            StoryStealthMode {
                active_until_date: 0,
                cooldown_until_date: 50,
            }
            .get_update_date(),
            50
        );
        assert_eq!(StoryStealthMode::default().get_update_date(), 0);
    }

    #[test]
    fn store_parse_round_trip() {
        // StoryStealthMode.hpp:14-51 — the two-date flag layout.
        let value = StoryStealthMode {
            active_until_date: 12345,
            cooldown_until_date: 0,
        };
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = StoryStealthMode::parse(&mut parser).expect("parse");
        assert_eq!(parsed, value);

        let value = StoryStealthMode {
            active_until_date: 0,
            cooldown_until_date: 98765,
        };
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = StoryStealthMode::parse(&mut parser).expect("parse");
        assert_eq!(parsed, value);
    }
}
