//! `TermsOfService` (td/telegram/TermsOfService.{h,cpp,hpp}) — the M.33
//! Wave-3 terms-of-service value class.

use crate::message_entity::{FormattedText, get_formatted_text, get_formatted_text_object};
use crate::store_parse::{Parser, store_bool, store_i32, store_string};
use crate::td_api;
use tdmtproto::telegram_api;

/// `td::TermsOfService` — the terms id + formatted text + age gate.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TermsOfService {
    id: String,
    text: FormattedText,
    min_user_age: i32,
    show_popup: bool,
}

impl TermsOfService {
    /// `TermsOfService(help_TermsOfService)` (TermsOfService.cpp:4-14) —
    /// the empty text clears the id (the C++ keeps the uninitialized
    /// defaults otherwise).
    pub fn from_telegram(terms: Option<&telegram_api::help_TermsOfService>) -> TermsOfService {
        let Some(terms) = terms else {
            return TermsOfService::default();
        };
        let mut id = terms
            .id
            .as_ref()
            .map_or(String::new(), |id| id.data.clone());
        let text = get_formatted_text::<()>(
            None,
            terms.text.clone(),
            terms.entities.clone(),
            true,
            false,
            "TermsOfService",
        );
        if text.text.is_empty() {
            id.clear();
        }
        TermsOfService {
            id,
            text,
            min_user_age: terms.min_age_confirm,
            show_popup: terms.popup,
        }
    }

    /// `get_id` (TermsOfService.h:11-13).
    pub fn get_id(&self) -> &str {
        &self.id
    }

    /// `get_terms_of_service_object` (TermsOfService.cpp:16-21) — `None`
    /// for the empty terms.
    pub fn get_terms_of_service_object(
        &self,
        user_manager: Option<&mut ()>,
    ) -> Option<td_api::TermsOfService> {
        if self.id.is_empty() {
            return None;
        }
        Some(td_api::TermsOfService {
            text: Some(get_formatted_text_object(
                user_manager,
                &self.text,
                true,
                -1,
            )),
            min_user_age: self.min_user_age,
            show_popup: self.show_popup,
        })
    }

    /// `TermsOfService::store` (TermsOfService.hpp).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_string(out, &self.id);
        self.text.store(out);
        store_i32(out, self.min_user_age);
        store_bool(out, self.show_popup);
    }

    /// `TermsOfService::parse` (TermsOfService.hpp).
    pub fn parse(parser: &mut Parser) -> Result<TermsOfService, &'static str> {
        Ok(TermsOfService {
            id: parser.string()?,
            text: FormattedText::parse(parser)?,
            min_user_age: parser.i32_()?,
            show_popup: parser.bool()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_parse;

    fn wire_terms(text: &str) -> telegram_api::help_TermsOfService {
        telegram_api::help_TermsOfService {
            flags: 0,
            popup: true,
            id: Some(telegram_api::DataJSON {
                data: "tos-id".to_string(),
            }),
            text: text.to_string(),
            entities: Vec::new(),
            min_age_confirm: 16,
        }
    }

    #[test]
    fn terms_from_the_wire_and_object_round_trip() {
        let terms = TermsOfService::from_telegram(Some(&wire_terms("hello")));
        assert_eq!(terms.get_id(), "tos-id");
        let object = terms.get_terms_of_service_object(None).unwrap();
        assert_eq!(object.min_user_age, 16);
        assert!(object.show_popup);
        assert_eq!(object.text.as_ref().unwrap().text, "hello");
    }

    #[test]
    fn empty_text_clears_the_id() {
        let mut wire = wire_terms("");
        wire.entities.clear();
        let terms = TermsOfService::from_telegram(Some(&wire));
        assert!(terms.get_id().is_empty());
        assert!(terms.get_terms_of_service_object(None).is_none());
    }

    #[test]
    fn null_terms_stay_empty() {
        let terms = TermsOfService::from_telegram(None);
        assert!(terms.get_id().is_empty());
        assert!(terms.get_terms_of_service_object(None).is_none());
    }

    #[test]
    fn store_parse_round_trips() {
        let terms = TermsOfService::from_telegram(Some(&wire_terms("some text")));
        let mut data = Vec::new();
        store_parse::store_log_event_version(&mut data);
        terms.store(&mut data);
        let mut parser = Parser::new_log_event(&data).unwrap();
        let parsed = TermsOfService::parse(&mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed, terms);
    }
}
