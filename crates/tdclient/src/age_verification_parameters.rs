//! `AgeVerificationParameters` (td/telegram/AgeVerificationParameters.{h,cpp,hpp})
//! — the M.33 Wave-3 account age-verification state, promoted from the
//! M.3.9.2 option-read stand-in (`crate::manager_option_states` re-exports
//! it until its consumers move).

use crate::store_parse::{Parser, store_i32, store_string};
use crate::td_api;

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
    pub(crate) fn get_age_verification_parameters_object(
        &self,
    ) -> Option<td_api::AgeVerificationParameters> {
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
    pub(crate) fn store(&self, out: &mut Vec<u8>) {
        let mut flags = 0i32;
        if self.need_verification {
            flags |= 1;
        }
        store_i32(out, flags);
        store_string(out, &self.bot_username);
        store_string(out, &self.country);
        store_i32(out, self.min_age);
    }

    /// `AgeVerificationParameters::parse` (AgeVerificationParameters.hpp:27-35).
    pub(crate) fn parse(input: &[u8]) -> Result<AgeVerificationParameters, &'static str> {
        let mut parser = Parser::new(input);
        let flags = parser.i32_()?;
        let bot_username = parser.string()?;
        let country = parser.string()?;
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
