//! `td::ReferralProgramManager` + the referral value family
//! (ReferralProgramManager.h/.cpp, ReferralProgramParameters.h/.hpp,
//! ReferralProgramInfo.h/.hpp, ReferralProgramSortOrder.h, M.52).
//!
//! The affiliate-program record plane with the byte-exact store/parse;
//! every server leg is a [blocked: net] seam with a parked promise.

use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::user_manager::UserId;
use tdutils::promise::Promise;

/// `td::ReferralProgramParameters` (ReferralProgramParameters.h/.hpp).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReferralProgramParameters {
    commission: i32,
    month_count: i32,
}

impl ReferralProgramParameters {
    /// `ReferralProgramParameters(int32, int32)`
    /// (ReferralProgramParameters.cpp:12-16).
    pub fn new(commission_permille: i32, duration_months: i32) -> ReferralProgramParameters {
        ReferralProgramParameters {
            commission: commission_permille,
            month_count: duration_months,
        }
    }

    /// `ReferralProgramParameters(affiliateProgramParameters)`
    /// (ReferralProgramParameters.cpp:18-26).
    pub fn from_td_api(
        parameters: Option<td_api::AffiliateProgramParameters>,
    ) -> ReferralProgramParameters {
        let Some(parameters) = parameters else {
            return ReferralProgramParameters::default();
        };
        ReferralProgramParameters {
            commission: parameters.commission_per_mille,
            month_count: parameters.month_count,
        }
    }

    /// `ReferralProgramParameters::is_valid`
    /// (ReferralProgramParameters.h:37-39).
    pub fn is_valid(self) -> bool {
        (1..=999).contains(&self.commission) && (0..=36).contains(&self.month_count)
    }

    /// `get_commission` (ReferralProgramParameters.h).
    pub fn get_commission(self) -> i32 {
        self.commission
    }

    /// `get_month_count` (ReferralProgramParameters.h).
    pub fn get_month_count(self) -> i32 {
        self.month_count
    }

    /// `get_affiliate_program_parameters_object`
    /// (ReferralProgramParameters.cpp:28-30).
    pub fn get_affiliate_program_parameters_object(self) -> td_api::AffiliateProgramParameters {
        td_api::AffiliateProgramParameters {
            commission_per_mille: self.commission,
            month_count: self.month_count,
        }
    }

    /// `store` (ReferralProgramParameters.hpp:13-22).
    pub fn store(self, out: &mut Vec<u8>) {
        let has_month_count = self.month_count != 0;
        store_parse::store_flags(out, [has_month_count]);
        store_parse::store_i32(out, self.commission);
        if has_month_count {
            store_parse::store_i32(out, self.month_count);
        }
    }

    /// `parse` (ReferralProgramParameters.hpp:23-35).
    pub fn parse(
        parser: &mut Parser,
    ) -> std::result::Result<ReferralProgramParameters, &'static str> {
        let [has_month_count] = parser.parse_flags()?;
        let result = ReferralProgramParameters {
            commission: parser.i32_()?,
            month_count: if has_month_count { parser.i32_()? } else { 0 },
        };
        if !result.is_valid() {
            return Err("Invalid referral program parameters stored in the database");
        }
        Ok(result)
    }
}

/// `td::ReferralProgramInfo` (ReferralProgramInfo.h/.hpp).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReferralProgramInfo {
    parameters: ReferralProgramParameters,
    end_date: i32,
    daily_star_count: i64,
}

impl ReferralProgramInfo {
    /// `ReferralProgramInfo(starRefProgram)` (ReferralProgramInfo.cpp:13-22).
    pub fn from_telegram(program: &tdmtproto::telegram_api::StarRefProgram) -> ReferralProgramInfo {
        let daily_star_count =
            program
                .daily_revenue_per_user
                .as_ref()
                .map_or(0, |amount| match amount {
                    tdmtproto::telegram_api::StarsAmount::starsAmount(amount) => amount.amount,
                    tdmtproto::telegram_api::StarsAmount::starsTonAmount(_) => 0,
                });
        ReferralProgramInfo {
            parameters: ReferralProgramParameters::new(
                program.commission_permille,
                program.duration_months,
            ),
            end_date: program.end_date,
            daily_star_count,
        }
    }

    /// `ReferralProgramInfo::is_valid` (ReferralProgramInfo.h:24-26).
    pub fn is_valid(self) -> bool {
        self.parameters.is_valid() && self.end_date >= 0
    }

    /// `ReferralProgramInfo::is_active` (ReferralProgramInfo.h:27-29).
    pub fn is_active(self) -> bool {
        self.end_date == 0
    }

    /// `get_affiliate_program_info_object` (ReferralProgramInfo.cpp:32-35).
    pub fn get_affiliate_program_info_object(self) -> td_api::AffiliateProgramInfo {
        td_api::AffiliateProgramInfo {
            parameters: Some(self.parameters.get_affiliate_program_parameters_object()),
            end_date: self.end_date,
            daily_revenue_per_user_amount: Some(td_api::StarAmount {
                star_count: self.daily_star_count,
                nanostar_count: 0,
            }),
        }
    }

    /// `store` (ReferralProgramInfo.hpp:13-21).
    pub fn store(self, out: &mut Vec<u8>) {
        let has_end_date = self.end_date != 0;
        let has_daily_star_count = self.daily_star_count != 0;
        store_parse::store_flags(out, [has_end_date, has_daily_star_count]);
        self.parameters.store(out);
        if has_end_date {
            store_parse::store_i32(out, self.end_date);
        }
        if has_daily_star_count {
            store_parse::store_i64(out, self.daily_star_count);
        }
    }

    /// `parse` (ReferralProgramInfo.hpp:22-33).
    pub fn parse(parser: &mut Parser) -> std::result::Result<ReferralProgramInfo, &'static str> {
        let [has_end_date, has_daily_star_count] = parser.parse_flags()?;
        Ok(ReferralProgramInfo {
            parameters: ReferralProgramParameters::parse(parser)?,
            end_date: if has_end_date { parser.i32_()? } else { 0 },
            daily_star_count: if has_daily_star_count {
                parser.i64_()?
            } else {
                0
            },
        })
    }
}

/// `td::ReferralProgramSortOrder` (ReferralProgramSortOrder.h:13).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReferralProgramSortOrder {
    #[default]
    Profitability,
    Date,
    Revenue,
}

impl ReferralProgramSortOrder {
    /// `get_referral_program_sort_order` (ReferralProgramSortOrder.cpp:9-18).
    pub fn from_td_api(
        order: Option<td_api::AffiliateProgramSortOrder>,
    ) -> ReferralProgramSortOrder {
        match order {
            Some(td_api::AffiliateProgramSortOrder::affiliateProgramSortOrderProfitability(_)) => {
                ReferralProgramSortOrder::Profitability
            }
            Some(td_api::AffiliateProgramSortOrder::affiliateProgramSortOrderCreationDate(_)) => {
                ReferralProgramSortOrder::Date
            }
            Some(td_api::AffiliateProgramSortOrder::affiliateProgramSortOrderRevenue(_)) => {
                ReferralProgramSortOrder::Revenue
            }
            None => ReferralProgramSortOrder::Profitability,
        }
    }
}

/// The `G()`/`td_` surface the referral plane reads through — every
/// server leg is a [blocked: net] seam with a parked promise.
pub trait ReferralProgramEnv {
    fn send_update(&mut self, update: td_api::Update) {
        tdutils::log_info!("send_update({update:?}) — seam default");
    }

    fn send_referral_query(&mut self, name: &str, promise: Promise<()>) {
        tdutils::log_info!("Send {name} — [blocked: net]");
        let _ = promise;
    }
    fn send_referral_query_chat(&mut self, name: &str, promise: Promise<Option<td_api::Chat>>) {
        tdutils::log_info!("Send {name} — [blocked: net]");
        let _ = promise;
    }
    fn send_referral_query_found_programs(
        &mut self,
        name: &str,
        promise: Promise<td_api::FoundAffiliatePrograms>,
    ) {
        tdutils::log_info!("Send {name} — [blocked: net]");
        let _ = promise;
    }
    fn send_referral_query_connected_program(
        &mut self,
        name: &str,
        promise: Promise<Option<td_api::ConnectedAffiliateProgram>>,
    ) {
        tdutils::log_info!("Send {name} — [blocked: net]");
        let _ = promise;
    }
    fn send_referral_query_connected_programs(
        &mut self,
        name: &str,
        promise: Promise<td_api::ConnectedAffiliatePrograms>,
    ) {
        tdutils::log_info!("Send {name} — [blocked: net]");
        let _ = promise;
    }
}

/// `td::ReferralProgramManager` (ReferralProgramManager.h:18-60).
#[derive(Default)]
pub struct ReferralProgramManager;

impl ReferralProgramManager {
    pub fn new() -> ReferralProgramManager {
        ReferralProgramManager
    }

    /// `set_dialog_referral_program` (ReferralProgramManager.cpp:362-366).
    pub fn set_dialog_referral_program(
        &mut self,
        env: &mut impl ReferralProgramEnv,
        _dialog_id: crate::dialog_id::DialogId,
        parameters: ReferralProgramParameters,
        promise: Promise<()>,
    ) {
        let _ = parameters;
        env.send_referral_query("set_dialog_referral_program", promise);
    }

    /// `search_dialog_referral_program` (ReferralProgramManager.cpp:368-372).
    pub fn search_dialog_referral_program(
        &mut self,
        env: &mut impl ReferralProgramEnv,
        _username: &str,
        _referral: &str,
        promise: Promise<Option<td_api::Chat>>,
    ) {
        env.send_referral_query_chat("search_dialog_referral_program", promise);
    }

    /// `search_referral_programs` (ReferralProgramManager.cpp:374-378).
    pub fn search_referral_programs(
        &mut self,
        env: &mut impl ReferralProgramEnv,
        _affiliate: Option<td_api::AffiliateType>,
        _sort_order: ReferralProgramSortOrder,
        _offset: &str,
        _limit: i32,
        promise: Promise<td_api::FoundAffiliatePrograms>,
    ) {
        env.send_referral_query_found_programs("search_referral_programs", promise);
    }

    /// `connect_referral_program` (ReferralProgramManager.cpp:380-384).
    pub fn connect_referral_program(
        &mut self,
        env: &mut impl ReferralProgramEnv,
        _affiliate: Option<td_api::AffiliateType>,
        bot_user_id: UserId,
        promise: Promise<Option<td_api::ConnectedAffiliateProgram>>,
    ) {
        let _ = bot_user_id;
        env.send_referral_query_connected_program("connect_referral_program", promise);
    }

    /// `revoke_referral_program` (ReferralProgramManager.cpp:386-390).
    pub fn revoke_referral_program(
        &mut self,
        env: &mut impl ReferralProgramEnv,
        _affiliate: Option<td_api::AffiliateType>,
        _url: &str,
        promise: Promise<Option<td_api::ConnectedAffiliateProgram>>,
    ) {
        env.send_referral_query_connected_program("revoke_referral_program", promise);
    }

    /// `get_connected_referral_program` (ReferralProgramManager.cpp:392-396).
    pub fn get_connected_referral_program(
        &mut self,
        env: &mut impl ReferralProgramEnv,
        _affiliate: Option<td_api::AffiliateType>,
        _bot_user_id: UserId,
        promise: Promise<Option<td_api::ConnectedAffiliateProgram>>,
    ) {
        env.send_referral_query_connected_program("get_connected_referral_program", promise);
    }

    /// `get_connected_referral_programs` (ReferralProgramManager.cpp:398-402).
    pub fn get_connected_referral_programs(
        &mut self,
        env: &mut impl ReferralProgramEnv,
        _affiliate: Option<td_api::AffiliateType>,
        _offset: &str,
        _limit: i32,
        promise: Promise<td_api::ConnectedAffiliatePrograms>,
    ) {
        env.send_referral_query_connected_programs("get_connected_referral_programs", promise);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_validation_and_round_trip() {
        let value = ReferralProgramParameters::new(500, 12);
        assert!(value.is_valid());
        assert!(!ReferralProgramParameters::new(0, 0).is_valid());
        assert!(!ReferralProgramParameters::new(1000, 0).is_valid());
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(
            ReferralProgramParameters::parse(&mut parser).unwrap(),
            value
        );
    }

    #[test]
    fn info_round_trip_and_activity() {
        let value = ReferralProgramInfo {
            parameters: ReferralProgramParameters::new(100, 3),
            end_date: 0,
            daily_star_count: 5,
        };
        assert!(value.is_active());
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(ReferralProgramInfo::parse(&mut parser).unwrap(), value);

        let value = ReferralProgramInfo {
            end_date: 100,
            ..value
        };
        assert!(!value.is_active());
    }

    #[test]
    fn sort_order_conversions() {
        assert_eq!(
            ReferralProgramSortOrder::from_td_api(Some(
                td_api::AffiliateProgramSortOrder::affiliateProgramSortOrderRevenue(
                    td_api::affiliateProgramSortOrderRevenue
                )
            )),
            ReferralProgramSortOrder::Revenue
        );
        assert_eq!(
            ReferralProgramSortOrder::from_td_api(None),
            ReferralProgramSortOrder::Profitability
        );
    }
}
