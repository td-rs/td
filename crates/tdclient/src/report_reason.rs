//! `ReportReason` (ReportReason.{h,cpp}) — the report-type + message pair
//! the report families carry (reportChatPhoto, reportChat, reportStory,
//! …). The td_api reason constructors collapse into the private enum; the
//! input form is the wire pair the `account.reportProfilePhoto`-style
//! queries carry.

use crate::td_api;
use tdmtproto::telegram_api;
use tdutils::status::{Result, Status};

/// `ReportReason` (ReportReason.h:18-52).
#[derive(Clone, Debug, Default)]
pub struct ReportReason {
    type_: Type,
    message_: String,
}

/// `ReportReason::Type` (ReportReason.h:19-31).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Type {
    #[default]
    Spam,
    Violence,
    Pornography,
    ChildAbuse,
    Copyright,
    UnrelatedLocation,
    Fake,
    IllegalDrugs,
    PersonalDetails,
    Custom,
}

impl ReportReason {
    /// `ReportReason::get_report_reason` (ReportReason.cpp:14-50) — the
    /// null reason is the C++ 400; the text must be clean UTF-8 input.
    pub fn get_report_reason(
        reason: Option<&td_api::ReportReason>,
        message: String,
    ) -> Result<ReportReason> {
        let Some(reason) = reason else {
            return Result::from_status(Status::error(400, "Chat report reason must be non-empty"));
        };
        if crate::misc::clean_input_string(&message).is_none() {
            return Result::from_status(Status::error(400, "Report text must be encoded in UTF-8"));
        }

        let type_ = match reason {
            td_api::ReportReason::reportReasonSpam(_) => Type::Spam,
            td_api::ReportReason::reportReasonViolence(_) => Type::Violence,
            td_api::ReportReason::reportReasonPornography(_) => Type::Pornography,
            td_api::ReportReason::reportReasonChildAbuse(_) => Type::ChildAbuse,
            td_api::ReportReason::reportReasonCopyright(_) => Type::Copyright,
            td_api::ReportReason::reportReasonUnrelatedLocation(_) => Type::UnrelatedLocation,
            td_api::ReportReason::reportReasonFake(_) => Type::Fake,
            td_api::ReportReason::reportReasonIllegalDrugs(_) => Type::IllegalDrugs,
            td_api::ReportReason::reportReasonPersonalDetails(_) => Type::PersonalDetails,
            td_api::ReportReason::reportReasonCustom(_) => Type::Custom,
        };
        Result::Ok(ReportReason {
            type_,
            message_: message,
        })
    }

    /// `ReportReason::get_input_report_reason` (ReportReason.cpp:52-71).
    pub fn get_input_report_reason(&self) -> telegram_api::ReportReason {
        use telegram_api::ReportReason as Wire;
        match self.type_ {
            Type::Spam => Wire::inputReportReasonSpam(telegram_api::inputReportReasonSpam),
            Type::Violence => {
                Wire::inputReportReasonViolence(telegram_api::inputReportReasonViolence)
            }
            Type::Pornography => {
                Wire::inputReportReasonPornography(telegram_api::inputReportReasonPornography)
            }
            Type::ChildAbuse => {
                Wire::inputReportReasonChildAbuse(telegram_api::inputReportReasonChildAbuse)
            }
            Type::Copyright => {
                Wire::inputReportReasonCopyright(telegram_api::inputReportReasonCopyright)
            }
            Type::UnrelatedLocation => {
                Wire::inputReportReasonGeoIrrelevant(telegram_api::inputReportReasonGeoIrrelevant)
            }
            Type::Fake => Wire::inputReportReasonFake(telegram_api::inputReportReasonFake),
            Type::IllegalDrugs => {
                Wire::inputReportReasonIllegalDrugs(telegram_api::inputReportReasonIllegalDrugs)
            }
            Type::PersonalDetails => Wire::inputReportReasonPersonalDetails(
                telegram_api::inputReportReasonPersonalDetails,
            ),
            Type::Custom => Wire::inputReportReasonOther(telegram_api::inputReportReasonOther),
        }
    }

    /// `ReportReason::get_message` (ReportReason.h:46-48).
    pub fn get_message(&self) -> &str {
        &self.message_
    }

    /// `ReportReason::is_spam` (ReportReason.h:50).
    pub fn is_spam(&self) -> bool {
        self.type_ == Type::Spam
    }

    /// `ReportReason::is_unrelated_location` (ReportReason.h:52).
    pub fn is_unrelated_location(&self) -> bool {
        self.type_ == Type::UnrelatedLocation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_reason_round_trips_the_ten_types() {
        // the null reason is the C++ 400
        let status = match ReportReason::get_report_reason(None, "x".to_owned()) {
            Result::Err(status) => status,
            Result::Ok(reason) => panic!("the null reason: {reason:?}"),
        };
        assert_eq!(status.message(), "Chat report reason must be non-empty");

        // (the "Report text must be encoded in UTF-8" band is the C++
        // byte-string guard — the Rust `String` is valid UTF-8 by
        // construction, so the band is unreachable through the td_api
        // layer and stays in the port as the faithful shape)

        let cases: [(td_api::ReportReason, telegram_api::ReportReason, &str); 10] = [
            (
                td_api::ReportReason::reportReasonSpam(td_api::reportReasonSpam),
                telegram_api::ReportReason::inputReportReasonSpam(
                    telegram_api::inputReportReasonSpam,
                ),
                "spam",
            ),
            (
                td_api::ReportReason::reportReasonViolence(td_api::reportReasonViolence),
                telegram_api::ReportReason::inputReportReasonViolence(
                    telegram_api::inputReportReasonViolence,
                ),
                "violence",
            ),
            (
                td_api::ReportReason::reportReasonPornography(td_api::reportReasonPornography),
                telegram_api::ReportReason::inputReportReasonPornography(
                    telegram_api::inputReportReasonPornography,
                ),
                "pornography",
            ),
            (
                td_api::ReportReason::reportReasonChildAbuse(td_api::reportReasonChildAbuse),
                telegram_api::ReportReason::inputReportReasonChildAbuse(
                    telegram_api::inputReportReasonChildAbuse,
                ),
                "child abuse",
            ),
            (
                td_api::ReportReason::reportReasonCopyright(td_api::reportReasonCopyright),
                telegram_api::ReportReason::inputReportReasonCopyright(
                    telegram_api::inputReportReasonCopyright,
                ),
                "copyright",
            ),
            (
                td_api::ReportReason::reportReasonUnrelatedLocation(
                    td_api::reportReasonUnrelatedLocation,
                ),
                telegram_api::ReportReason::inputReportReasonGeoIrrelevant(
                    telegram_api::inputReportReasonGeoIrrelevant,
                ),
                "unrelated location",
            ),
            (
                td_api::ReportReason::reportReasonFake(td_api::reportReasonFake),
                telegram_api::ReportReason::inputReportReasonFake(
                    telegram_api::inputReportReasonFake,
                ),
                "fake",
            ),
            (
                td_api::ReportReason::reportReasonIllegalDrugs(td_api::reportReasonIllegalDrugs),
                telegram_api::ReportReason::inputReportReasonIllegalDrugs(
                    telegram_api::inputReportReasonIllegalDrugs,
                ),
                "illegal drugs",
            ),
            (
                td_api::ReportReason::reportReasonPersonalDetails(
                    td_api::reportReasonPersonalDetails,
                ),
                telegram_api::ReportReason::inputReportReasonPersonalDetails(
                    telegram_api::inputReportReasonPersonalDetails,
                ),
                "personal details",
            ),
            (
                td_api::ReportReason::reportReasonCustom(td_api::reportReasonCustom),
                telegram_api::ReportReason::inputReportReasonOther(
                    telegram_api::inputReportReasonOther,
                ),
                "custom",
            ),
        ];
        for (td_reason, wire, name) in cases {
            let reason = match ReportReason::get_report_reason(Some(&td_reason), name.to_owned()) {
                Result::Ok(reason) => reason,
                Result::Err(status) => panic!("the {name} reason: {status}"),
            };
            assert_eq!(reason.get_message(), name);
            assert!(matches!(
                reason.get_input_report_reason(),
                wire_variant if std::mem::discriminant(&wire_variant) == std::mem::discriminant(&wire)
            ));
        }

        // the spam/unrelated-location predicates
        let spam = match ReportReason::get_report_reason(
            Some(&td_api::ReportReason::reportReasonSpam(
                td_api::reportReasonSpam,
            )),
            String::new(),
        ) {
            Result::Ok(reason) => reason,
            Result::Err(status) => panic!("the spam reason: {status}"),
        };
        assert!(spam.is_spam());
        assert!(!spam.is_unrelated_location());
        let location = match ReportReason::get_report_reason(
            Some(&td_api::ReportReason::reportReasonUnrelatedLocation(
                td_api::reportReasonUnrelatedLocation,
            )),
            String::new(),
        ) {
            Result::Ok(reason) => reason,
            Result::Err(status) => panic!("the location reason: {status}"),
        };
        assert!(location.is_unrelated_location());
        assert!(!location.is_spam());
    }
}
