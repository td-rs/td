//! `WebDomainException` (td/telegram/WebDomainException.{h,cpp,hpp}) +
//! `WebBrowserSettings` (td/telegram/WebBrowserSettings.{h,cpp,hpp}) — the
//! M.31 Wave-3 value plane for the in-app web browser settings.

use crate::emoji_status::CustomEmojiId;
use crate::store_parse::{self, Parser};
use crate::td_api;
use tdmtproto::telegram_api;

/// `td::WebDomainException` (WebDomainException.h) — a domain exception of
/// the web browser settings. The C++ `StringBuilder& operator<<` collapses
/// to the derived `Debug`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebDomainException {
    domain: String,
    url: String,
    title: String,
    favicon_custom_emoji_id: CustomEmojiId,
}

impl WebDomainException {
    /// `WebDomainException(telegram_api::webDomainException&&)`
    /// (WebDomainException.cpp:10-20).
    pub fn from_telegram(exception: telegram_api::WebDomainException) -> WebDomainException {
        let mut favicon_custom_emoji_id = CustomEmojiId::new(exception.favicon);
        if !favicon_custom_emoji_id.is_valid()
            && favicon_custom_emoji_id != CustomEmojiId::default()
        {
            tdutils::log_error!("Receive favicon {favicon_custom_emoji_id:?}");
            favicon_custom_emoji_id = CustomEmojiId::default();
        }
        WebDomainException {
            domain: exception.domain,
            url: exception.url,
            title: exception.title,
            favicon_custom_emoji_id,
        }
    }

    /// `has_domain` (WebDomainException.h:20-22).
    pub fn has_domain(&self, domain: &str) -> bool {
        self.domain == domain
    }

    /// `has_domain_or_subdomain` (WebDomainException.cpp:22-25) — the
    /// byte-wise `ends_with` + dot-boundary check of the C++.
    pub fn has_domain_or_subdomain(&self, domain: &str) -> bool {
        domain.ends_with(self.domain.as_str())
            && (domain.len() == self.domain.len()
                || domain.as_bytes()[domain.len() - self.domain.len() - 1] == b'.')
    }

    /// `get_web_domain_exception_object` (WebDomainException.cpp:27-30).
    pub fn get_web_domain_exception_object(&self) -> td_api::WebDomainException {
        td_api::WebDomainException {
            url: self.url.clone(),
            domain: self.domain.clone(),
            title: if self.title.is_empty() {
                self.domain.clone()
            } else {
                self.title.clone()
            },
            favicon_custom_emoji_id: self.favicon_custom_emoji_id.get(),
        }
    }

    /// `get_web_domain_exceptions` (WebDomainException.cpp:32-38).
    pub fn get_web_domain_exceptions(
        exceptions: Vec<telegram_api::WebDomainException>,
    ) -> Vec<WebDomainException> {
        exceptions
            .into_iter()
            .map(WebDomainException::from_telegram)
            .collect()
    }

    /// `get_web_domain_exceptions_object` (WebDomainException.cpp:40-45).
    pub fn get_web_domain_exceptions_object(
        exceptions: &[WebDomainException],
    ) -> Vec<td_api::WebDomainException> {
        exceptions
            .iter()
            .map(WebDomainException::get_web_domain_exception_object)
            .collect()
    }

    /// `WebDomainException::store` (WebDomainException.hpp).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_favicon_custom_emoji_id = self.favicon_custom_emoji_id.is_valid();
        store_parse::store_flags(out, [has_favicon_custom_emoji_id]);
        store_parse::store_string(out, &self.domain);
        store_parse::store_string(out, &self.url);
        store_parse::store_string(out, &self.title);
        if has_favicon_custom_emoji_id {
            store_parse::store_i64(out, self.favicon_custom_emoji_id.get());
        }
    }

    /// `WebDomainException::parse` (WebDomainException.hpp).
    pub fn parse(parser: &mut Parser) -> Result<WebDomainException, &'static str> {
        let [has_favicon_custom_emoji_id] = parser.parse_flags()?;
        let domain = parser.string()?;
        let url = parser.string()?;
        let title = parser.string()?;
        let favicon_custom_emoji_id = if has_favicon_custom_emoji_id {
            CustomEmojiId::new(parser.i64_()?)
        } else {
            CustomEmojiId::default()
        };
        Ok(WebDomainException {
            domain,
            url,
            title,
            favicon_custom_emoji_id,
        })
    }
}

/// `td::WebBrowserSettings` (WebBrowserSettings.h) — the web browser
/// settings with the domain exceptions. The wire union
/// `account_WebBrowserSettings` is split by constructor before it reaches
/// the value (see [`crate::web_browser_manager::WebBrowserSettingsAnswer`]),
/// so the value only models the settings variant.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebBrowserSettings {
    external_exceptions: Vec<WebDomainException>,
    inapp_exceptions: Vec<WebDomainException>,
    open_external_browser: bool,
    display_close_button: bool,
    hash: i64,
}

impl WebBrowserSettings {
    /// `WebBrowserSettings(account_webBrowserSettings&&)`
    /// (WebBrowserSettings.cpp:16-28); the `NotModified` branch lives in
    /// the answer split.
    pub fn from_telegram(settings: telegram_api::account_webBrowserSettings) -> WebBrowserSettings {
        WebBrowserSettings {
            open_external_browser: settings.open_external_browser,
            display_close_button: settings.display_close_button,
            external_exceptions: WebDomainException::get_web_domain_exceptions(
                settings.external_exceptions,
            ),
            inapp_exceptions: WebDomainException::get_web_domain_exceptions(
                settings.inapp_exceptions,
            ),
            hash: settings.hash,
        }
    }

    /// `get_web_browser_settings_object` (WebBrowserSettings.cpp:30-34).
    pub fn get_web_browser_settings_object(&self) -> td_api::WebBrowserSettings {
        td_api::WebBrowserSettings {
            open_external_browser: self.open_external_browser,
            external_exceptions: WebDomainException::get_web_domain_exceptions_object(
                &self.external_exceptions,
            ),
            in_app_exceptions: WebDomainException::get_web_domain_exceptions_object(
                &self.inapp_exceptions,
            ),
            display_close_button: self.display_close_button,
        }
    }

    /// `get_hash` (WebBrowserSettings.h:31-33).
    pub fn get_hash(&self) -> i64 {
        self.hash
    }

    /// `update_from(updateWebBrowserSettings)` (WebBrowserSettings.cpp:36-44).
    pub fn update_from(&mut self, update: telegram_api::updateWebBrowserSettings) -> bool {
        if self.open_external_browser == update.open_external_browser
            && self.display_close_button == update.display_close_button
        {
            return false;
        }
        self.open_external_browser = update.open_external_browser;
        self.display_close_button = update.display_close_button;
        true
    }

    /// `update_from(updateWebBrowserException)` (WebBrowserSettings.cpp:46-72).
    pub fn update_from_exception(
        &mut self,
        update: telegram_api::updateWebBrowserException,
    ) -> bool {
        let mut result = false;
        let mut is_found = false;
        let Some(exception) = update.exception.as_ref() else {
            // the C++ dereferences the exception unconditionally (server
            // trust); the null object drops the update instead.
            return false;
        };
        if let Some(index) = self
            .external_exceptions
            .iter()
            .position(|current| current.has_domain(&exception.domain))
        {
            if update.delete || !update.open_external_browser {
                self.external_exceptions.remove(index);
                result = true;
            } else {
                is_found = true;
            }
        }
        if let Some(index) = self
            .inapp_exceptions
            .iter()
            .position(|current| current.has_domain(&exception.domain))
        {
            if update.delete || update.open_external_browser {
                self.inapp_exceptions.remove(index);
                result = true;
            } else {
                is_found = true;
            }
        }
        if !update.delete && !is_found {
            let exception = WebDomainException::from_telegram(
                update
                    .exception
                    .expect("updateWebBrowserException.exception is non-empty"),
            );
            if update.open_external_browser {
                self.external_exceptions.push(exception);
            } else {
                self.inapp_exceptions.push(exception);
            }
            result = true;
        }
        result
    }

    /// `get_open_external_browser` (WebBrowserSettings.cpp:74-90).
    pub fn get_open_external_browser(&self, domain: &str) -> bool {
        if domain.ends_with(".magic.org") {
            return false;
        }
        if self.open_external_browser {
            for exception in &self.inapp_exceptions {
                if exception.has_domain_or_subdomain(domain) {
                    return false;
                }
            }
        } else {
            for exception in &self.external_exceptions {
                if exception.has_domain_or_subdomain(domain) {
                    return true;
                }
            }
        }
        self.open_external_browser
    }

    /// `WebBrowserSettings::store` (WebBrowserSettings.hpp).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_external_exceptions = !self.external_exceptions.is_empty();
        let has_inapp_exceptions = !self.inapp_exceptions.is_empty();
        store_parse::store_flags(
            out,
            [
                self.open_external_browser,
                self.display_close_button,
                has_external_exceptions,
                has_inapp_exceptions,
            ],
        );
        if has_external_exceptions {
            store_parse::store_i32(out, self.external_exceptions.len() as i32);
            for exception in &self.external_exceptions {
                exception.store(out);
            }
        }
        if has_inapp_exceptions {
            store_parse::store_i32(out, self.inapp_exceptions.len() as i32);
            for exception in &self.inapp_exceptions {
                exception.store(out);
            }
        }
        store_parse::store_i64(out, self.hash);
    }

    /// `WebBrowserSettings::parse` (WebBrowserSettings.hpp).
    pub fn parse(parser: &mut Parser) -> Result<WebBrowserSettings, &'static str> {
        let [
            open_external_browser,
            display_close_button,
            has_external_exceptions,
            has_inapp_exceptions,
        ] = parser.parse_flags()?;
        let external_exceptions = if has_external_exceptions {
            parse_web_domain_exceptions(parser)?
        } else {
            Vec::new()
        };
        let inapp_exceptions = if has_inapp_exceptions {
            parse_web_domain_exceptions(parser)?
        } else {
            Vec::new()
        };
        let hash = parser.i64_()?;
        Ok(WebBrowserSettings {
            external_exceptions,
            inapp_exceptions,
            open_external_browser,
            display_close_button,
            hash,
        })
    }
}

/// `td::parse(vector<WebDomainException>)` — the count-prefixed item loop
/// (VectorHelper).
fn parse_web_domain_exceptions(
    parser: &mut Parser,
) -> Result<Vec<WebDomainException>, &'static str> {
    let size = parser.i32_()?;
    if size < 0 {
        return Err("invalid web domain exceptions count");
    }
    let mut exceptions = Vec::with_capacity(size as usize);
    for _ in 0..size {
        exceptions.push(WebDomainException::parse(parser)?);
    }
    Ok(exceptions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_parse::Parser;

    fn wire_exception(domain: &str, favicon: i64) -> telegram_api::WebDomainException {
        telegram_api::WebDomainException {
            flags: 0,
            domain: domain.to_string(),
            url: format!("https://{domain}"),
            title: format!("Title {domain}"),
            favicon,
        }
    }

    fn round_trip(settings: &WebBrowserSettings) -> WebBrowserSettings {
        let mut data = Vec::new();
        store_parse::store_log_event_version(&mut data);
        settings.store(&mut data);
        let mut parser = Parser::new_log_event(&data).unwrap();
        let parsed = WebBrowserSettings::parse(&mut parser).unwrap();
        parser.fetch_end().unwrap();
        parsed
    }

    #[test]
    fn web_domain_exception_object_uses_domain_as_title_when_empty() {
        let exception = WebDomainException::from_telegram(telegram_api::WebDomainException {
            title: String::new(),
            ..wire_exception("example.com", 0)
        });
        let object = exception.get_web_domain_exception_object();
        assert_eq!(object.domain, "example.com");
        assert_eq!(object.title, "example.com");
        assert_eq!(object.favicon_custom_emoji_id, 0);
    }

    #[test]
    fn web_domain_exception_keeps_a_nonzero_favicon() {
        // CustomEmojiId::is_valid() is `id != 0` (CustomEmojiId.h), so the
        // C++ reset branch is unreachable (0 fails both guards); any
        // nonzero favicon passes through unchanged.
        let exception = WebDomainException::from_telegram(wire_exception("example.com", 5));
        assert_eq!(exception.favicon_custom_emoji_id.get(), 5);
        let exception = WebDomainException::from_telegram(wire_exception("example.com", -100));
        assert_eq!(exception.favicon_custom_emoji_id.get(), -100);
    }

    #[test]
    fn has_domain_or_subdomain_matches_boundaries() {
        let exception = WebDomainException::from_telegram(wire_exception("example.com", 0));
        assert!(exception.has_domain_or_subdomain("example.com"));
        assert!(exception.has_domain_or_subdomain("sub.example.com"));
        assert!(!exception.has_domain_or_subdomain("notexample.com"));
        assert!(!exception.has_domain_or_subdomain("example.com.evil"));
    }

    #[test]
    fn web_browser_settings_store_parse_round_trips() {
        let mut settings = WebBrowserSettings {
            open_external_browser: true,
            display_close_button: true,
            hash: 12345,
            ..WebBrowserSettings::default()
        };
        settings
            .external_exceptions
            .push(WebDomainException::from_telegram(wire_exception(
                "external.example",
                -7,
            )));
        settings
            .inapp_exceptions
            .push(WebDomainException::from_telegram(wire_exception(
                "inapp.example",
                0,
            )));
        assert_eq!(round_trip(&settings), settings);

        let empty = WebBrowserSettings::default();
        assert_eq!(round_trip(&empty), empty);
    }

    #[test]
    fn update_from_settings_returns_change_flag() {
        let mut settings = WebBrowserSettings::default();
        assert!(
            !settings.update_from(telegram_api::updateWebBrowserSettings {
                flags: 0,
                open_external_browser: false,
                display_close_button: false,
            })
        );
        assert!(
            settings.update_from(telegram_api::updateWebBrowserSettings {
                flags: 0,
                open_external_browser: true,
                display_close_button: false,
            })
        );
        assert!(settings.open_external_browser);
        assert!(!settings.display_close_button);
    }

    fn exception_update(
        domain: &str,
        delete: bool,
        open_external_browser: bool,
    ) -> telegram_api::updateWebBrowserException {
        telegram_api::updateWebBrowserException {
            flags: 0,
            exception: Some(wire_exception(domain, 0)),
            delete,
            open_external_browser,
        }
    }

    #[test]
    fn update_from_exception_adds_to_the_right_list() {
        let mut settings = WebBrowserSettings::default();
        assert!(settings.update_from_exception(exception_update("a.com", false, true)));
        assert!(settings.update_from_exception(exception_update("b.com", false, false)));
        assert_eq!(settings.external_exceptions.len(), 1);
        assert_eq!(settings.inapp_exceptions.len(), 1);

        // the same domain in the same list is a no-op
        assert!(!settings.update_from_exception(exception_update("a.com", false, true)));

        // deletion removes from the external list
        assert!(settings.update_from_exception(exception_update("a.com", true, true)));
        assert!(settings.external_exceptions.is_empty());

        // an in-app exception moving to the external list
        assert!(settings.update_from_exception(exception_update("b.com", false, true)));
        assert!(settings.inapp_exceptions.is_empty());
        assert_eq!(settings.external_exceptions.len(), 1);
    }

    #[test]
    fn get_open_external_browser_applies_exceptions() {
        let mut settings = WebBrowserSettings {
            open_external_browser: false,
            ..WebBrowserSettings::default()
        };
        assert!(!settings.get_open_external_browser("example.com"));
        assert!(!settings.get_open_external_browser("x.magic.org"));

        settings
            .external_exceptions
            .push(WebDomainException::from_telegram(wire_exception(
                "docs.example.com",
                0,
            )));
        assert!(settings.get_open_external_browser("docs.example.com"));
        assert!(settings.get_open_external_browser("deep.docs.example.com"));
        assert!(!settings.get_open_external_browser("example.com"));

        settings.open_external_browser = true;
        settings.external_exceptions.clear();
        settings
            .inapp_exceptions
            .push(WebDomainException::from_telegram(wire_exception(
                "app.example.com",
                0,
            )));
        assert!(settings.get_open_external_browser("example.com"));
        assert!(!settings.get_open_external_browser("app.example.com"));
    }
}
