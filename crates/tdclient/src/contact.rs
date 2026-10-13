//! Port of `td/telegram/Contact.{h,cpp}` (tdlib 1.8.67) — the subset the
//! contacts slice (UserManager M.4.14) needs.
//!
//! Mapping notes:
//!
//! - The `edit_note_`/`note_` pair is deliberately not ported: the C++
//!   `store` skips both ("don't need to store edit_note and note"), and
//!   the only consumer (`get_input_phone_contact`'s
//!   `get_input_text_with_entities`, via the `addContact` note branch)
//!   funnels through `FormattedText` — the MessageEntity step.
//! - The C++ `operator==` compares the full 5-tuple (phone number, first
//!   name, last name, vCard, user id); the import dedup uses
//!   `ContactHash`/`ContactEqual` — the 3-tuple without vCard and user id.
//!   Rust derives `PartialEq` for the 5-tuple and the dedup keys the
//!   import map on the 3-tuple directly.
//! - `get_input_phone_contact`/`get_contact_object` are the query
//!   and Files halves — they arrive with the net step and the requests
//!   wiring. `get_input_media_contact`/`get_secret_input_media_contact`
//!   landed with the M.9.6 upload/secret media bridges.
//! - The td_api `importedContact` → `Contact` conversion (`get_contact`)
//!   is the requests-wiring half (M.4.16); the constructors and
//!   `validate` are here because the import machine manipulates
//!   `Contact`s directly.

use crate::store_parse;
use crate::td_api;
use crate::user_manager::UserId;
use tdutils::status::Status;

/// `td::Contact`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Contact {
    phone_number: String,
    first_name: String,
    last_name: String,
    vcard: String,
    user_id: UserId,
}

impl Contact {
    /// `Contact::Contact(phone_number, first_name, last_name, vcard,
    /// user_id)` — an invalid user id collapses to the default, like the
    /// C++.
    pub fn new(
        phone_number: String,
        first_name: String,
        last_name: String,
        vcard: String,
        user_id: UserId,
    ) -> Contact {
        Contact {
            phone_number,
            first_name,
            last_name,
            vcard,
            user_id: if user_id.is_valid() {
                user_id
            } else {
                UserId::default()
            },
        }
    }

    /// `Contact::validate` (Contact.cpp:37-52) — the `clean_input_string`
    /// pass over every field with the exact C++ error strings.
    pub fn validate(&mut self) -> Result<(), Status> {
        if let Some(cleaned) = crate::misc::clean_input_string(&self.phone_number) {
            self.phone_number = cleaned;
        } else {
            return Err(Status::error(400, "Phone number must be encoded in UTF-8"));
        }
        if let Some(cleaned) = crate::misc::clean_input_string(&self.first_name) {
            self.first_name = cleaned;
        } else {
            return Err(Status::error(400, "First name must be encoded in UTF-8"));
        }
        if let Some(cleaned) = crate::misc::clean_input_string(&self.last_name) {
            self.last_name = cleaned;
        } else {
            return Err(Status::error(400, "Last name must be encoded in UTF-8"));
        }
        if let Some(cleaned) = crate::misc::clean_input_string(&self.vcard) {
            self.vcard = cleaned;
        } else {
            return Err(Status::error(400, "vCard must be encoded in UTF-8"));
        }
        Ok(())
    }

    pub fn set_user_id(&mut self, user_id: UserId) {
        self.user_id = user_id;
    }

    pub fn get_user_id(&self) -> UserId {
        self.user_id
    }

    /// `Contact::get_input_phone_contact` (Contact.cpp:90-101) — the
    /// `contacts.importContacts` payload. The `edit_note_` note branch is
    /// the formatted-text plane (the M.4.14 never-stored divergence), so
    /// `flags` stays 0 and the note never rides.
    pub fn get_input_phone_contact(&self, client_id: i64) -> tdmtproto::telegram_api::InputContact {
        tdmtproto::telegram_api::InputContact {
            flags: 0,
            client_id,
            phone: self.phone_number.clone(),
            first_name: self.first_name.clone(),
            last_name: self.last_name.clone(),
            note: None,
        }
    }

    /// `Contact::get_input_media_contact` (Contact.cpp:81-83).
    pub fn get_input_media_contact(&self) -> Option<tdmtproto::telegram_api::InputMedia> {
        Some(tdmtproto::telegram_api::InputMedia::inputMediaContact(
            tdmtproto::telegram_api::inputMediaContact {
                phone_number: self.phone_number.clone(),
                first_name: self.first_name.clone(),
                last_name: self.last_name.clone(),
                vcard: self.vcard.clone(),
            },
        ))
    }

    /// `Contact::get_secret_input_media_contact` (Contact.cpp:85-88) — the
    /// `SecretInputMedia` wrapper lands with the M.9.6 plane; the contact
    /// half answers the decrypted media directly.
    pub fn get_secret_input_media_contact(
        &self,
    ) -> Option<tdmtproto::secret_api::DecryptedMessageMedia> {
        Some(
            tdmtproto::secret_api::DecryptedMessageMedia::decryptedMessageMediaContact(
                tdmtproto::secret_api::decryptedMessageMediaContact {
                    phone_number: self.phone_number.as_bytes().to_vec(),
                    first_name: self.first_name.as_bytes().to_vec(),
                    last_name: self.last_name.as_bytes().to_vec(),
                    user_id: 0,
                },
            ),
        )
    }

    pub fn get_phone_number(&self) -> &str {
        &self.phone_number
    }

    pub fn get_first_name(&self) -> &str {
        &self.first_name
    }

    pub fn get_last_name(&self) -> &str {
        &self.last_name
    }

    /// `Contact::get_vcard` (Contact.h:83).
    pub fn get_vcard(&self) -> &str {
        &self.vcard
    }

    /// `Contact::get_contact_object` (Contact.cpp:142-149) — the td_api
    /// builder (the M.21 InlineQueriesManager intake is the consumer that
    /// needs it before the net step).
    pub fn get_contact_object(&self) -> td_api::Contact {
        td_api::Contact {
            phone_number: self.phone_number.clone(),
            first_name: self.first_name.clone(),
            last_name: self.last_name.clone(),
            vcard: self.vcard.clone(),
            user_id: self.user_id.get(),
        }
    }

    /// `Contact::store` (Contact.h:87-108) — the flag block gates the
    /// three optional strings and the user id; `edit_note_`/`note_` are
    /// never stored, like the C++.
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_first_name = !self.first_name.is_empty();
        let has_last_name = !self.last_name.is_empty();
        let has_vcard = !self.vcard.is_empty();
        let has_user_id = self.user_id.is_valid();
        store_parse::store_flags(out, [has_first_name, has_last_name, has_vcard, has_user_id]);
        store_parse::store_string(out, &self.phone_number);
        if has_first_name {
            store_parse::store_string(out, &self.first_name);
        }
        if has_last_name {
            store_parse::store_string(out, &self.last_name);
        }
        if has_vcard {
            store_parse::store_string(out, &self.vcard);
        }
        if has_user_id {
            store_parse::store_i64(out, self.user_id.get());
        }
    }

    /// `Contact::parse` (Contact.h:119-139) — the pre-`AddContactVcard`
    /// blobs have no flag block: first/last name present, no vCard, user
    /// id present.
    pub fn parse_from(parser: &mut store_parse::Parser) -> Result<Contact, &'static str> {
        let mut has_first_name = true;
        let mut has_last_name = true;
        let mut has_vcard = false;
        let mut has_user_id = true;
        if parser.version() >= store_parse::version::ADD_CONTACT_VCARD {
            [has_first_name, has_last_name, has_vcard, has_user_id] = parser.parse_flags()?;
        }
        let phone_number = parser.string()?;
        let first_name = if has_first_name {
            parser.string()?
        } else {
            String::new()
        };
        let last_name = if has_last_name {
            parser.string()?
        } else {
            String::new()
        };
        let vcard = if has_vcard {
            parser.string()?
        } else {
            String::new()
        };
        let user_id = if has_user_id {
            UserId::new(parser.i64_()?)
        } else {
            UserId::default()
        };
        Ok(Contact {
            phone_number,
            first_name,
            last_name,
            vcard,
            user_id,
        })
    }

    /// `Contact::parse` over a standalone blob (the log-event framing is
    /// handled by the caller — `parse_from` reads the payload fields).
    pub fn parse(input: &[u8]) -> Result<Contact, &'static str> {
        let mut parser = store_parse::Parser::new(input);
        let contact = Contact::parse_from(&mut parser)?;
        parser.fetch_end()?;
        Ok(contact)
    }

    /// `ContactEqual`/`ContactHash` (Contact.h:146-158) — the import dedup
    /// key: phone number, first name and last name only.
    pub fn get_dedup_key(&self) -> (String, String, String) {
        (
            self.phone_number.clone(),
            self.first_name.clone(),
            self.last_name.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_contact() -> Contact {
        Contact::new(
            "+123".to_owned(),
            "Alice".to_owned(),
            "Cooper".to_owned(),
            "vcard".to_owned(),
            UserId::new(7),
        )
    }

    #[test]
    fn store_parse_round_trips_the_full_contact() {
        let contact = full_contact();
        let mut out = Vec::new();
        contact.store(&mut out);
        let parsed = Contact::parse(&out).expect("parses");
        assert_eq!(parsed, contact);
    }

    #[test]
    fn store_parse_omits_the_empty_optionals() {
        let contact = Contact::new(
            "+1".to_owned(),
            String::new(),
            String::new(),
            String::new(),
            UserId::default(),
        );
        let mut out = Vec::new();
        contact.store(&mut out);
        // the flag word is all-false
        assert_eq!(&out[..4], &[0, 0, 0, 0]);
        let parsed = Contact::parse(&out).expect("parses");
        assert_eq!(parsed, contact);
    }

    #[test]
    fn parse_accepts_pre_add_contact_vcard_blobs() {
        // version 15 predates AddContactVcard (16): no flag block, first
        // and last names and the user id are always present
        let mut out = Vec::new();
        store_parse::store_i32(&mut out, 15);
        store_parse::store_string(&mut out, "+1");
        store_parse::store_string(&mut out, "Alice");
        store_parse::store_string(&mut out, "Cooper");
        store_parse::store_i64(&mut out, 7);
        let mut parser = store_parse::Parser::new_log_event(&out).expect("valid log event");
        let parsed = Contact::parse_from(&mut parser).expect("parses");
        assert_eq!(parsed.phone_number, "+1");
        assert_eq!(parsed.first_name, "Alice");
        assert_eq!(parsed.last_name, "Cooper");
        assert_eq!(parsed.user_id, UserId::new(7));
        assert!(parsed.vcard.is_empty());
        parser.fetch_end().expect("the stream is fully consumed");
    }

    #[test]
    fn equality_is_the_full_tuple_but_the_dedup_key_is_three_fields() {
        let a = full_contact();
        let mut b = full_contact();
        assert_eq!(a, b);
        // the vCard and the user id do not take part in the import dedup
        b.vcard = "other".to_owned();
        b.user_id = UserId::new(9);
        assert_ne!(a, b);
        assert_eq!(a.get_dedup_key(), b.get_dedup_key());
    }

    #[test]
    fn validate_rejects_invalid_utf8_with_the_cpp_errors() {
        let mut contact = Contact::new(
            // deliberately invalid UTF-8, like a broken wire record
            unsafe { String::from_utf8_unchecked(vec![0xff]) },
            "Alice".to_owned(),
            String::new(),
            String::new(),
            UserId::default(),
        );
        let error = contact.validate().expect_err("invalid utf-8 rejects");
        assert_eq!(error.code(), 400);
        assert_eq!(error.message(), "Phone number must be encoded in UTF-8");
    }
}
