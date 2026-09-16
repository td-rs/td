//! Port of `td/telegram/RestrictionReason.{h,cpp}`: the restriction list
//! attached to users, the platform-aware lookup (`get_restriction_reason`)
//! and the td_api `restrictionInfo` conversion. The legacy
//! colon-dash-string parser is kept for old database values.

use crate::store_parse::{Parser, store_string};
use crate::td_api;
use tdmtproto::telegram_api;

/// `td::RestrictionReason`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RestrictionReason {
    platform: String,
    reason: String,
    description: String,
}

impl RestrictionReason {
    pub fn new(platform: String, reason: String, description: String) -> RestrictionReason {
        let description = if description.is_empty() {
            reason.clone()
        } else {
            description
        };
        RestrictionReason {
            platform,
            reason,
            description,
        }
    }

    /// `RestrictionReason::get_platform`.
    pub fn platform(&self) -> &str {
        &self.platform
    }

    fn is_sensitive(&self) -> bool {
        self.reason == "sensitive"
    }

    /// `RestrictionReason::store` (RestrictionReason.h).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_string(out, &self.platform);
        store_string(out, &self.reason);
        store_string(out, &self.description);
    }

    /// `RestrictionReason::parse` (RestrictionReason.h).
    pub fn parse_vec(input: &[u8]) -> Result<Vec<RestrictionReason>, &'static str> {
        let mut parser = Parser::new(input);
        let len = parser.i32_()?;
        if len < 0 {
            return Err("Wrong vector length");
        }
        let mut result = Vec::with_capacity(len as usize);
        for _ in 0..len {
            let platform = parser.string()?;
            let reason = parser.string()?;
            let description = parser.string()?;
            result.push(RestrictionReason {
                platform,
                reason,
                description,
            });
        }
        parser.fetch_end()?;
        Ok(result)
    }
}

/// `td::store(vector<RestrictionReason>)`.
pub fn store_restriction_reasons(out: &mut Vec<u8>, reasons: &[RestrictionReason]) {
    crate::store_parse::store_i32(out, reasons.len() as i32);
    for reason in reasons {
        reason.store(out);
    }
}

pub fn parse_restriction_reasons(
    parser: &mut Parser,
) -> Result<Vec<RestrictionReason>, &'static str> {
    let len = parser.i32_()?;
    if len < 0 {
        return Err("Wrong vector length");
    }
    let mut result = Vec::with_capacity(len as usize);
    for _ in 0..len {
        let platform = parser.string()?;
        let reason = parser.string()?;
        let description = parser.string()?;
        result.push(RestrictionReason {
            platform,
            reason,
            description,
        });
    }
    Ok(result)
}

/// The option lookups `get_restriction_reason` performs through `G()`.
pub struct RestrictionOptions {
    pub ignored_restriction_reasons: String,
    pub restriction_add_platforms: String,
    pub ignore_platform_restrictions: bool,
}

/// `td::get_restriction_reason` (RestrictionReason.cpp:20-68): the current
/// platform is checked first, then the added platforms, then "all".
pub fn get_restriction_reason<'a>(
    restriction_reasons: &'a [RestrictionReason],
    options: &RestrictionOptions,
    sensitive: bool,
) -> Option<&'a RestrictionReason> {
    if restriction_reasons.is_empty() {
        return None;
    }

    let ignored_restriction_reasons: Vec<&str> =
        full_split(&options.ignored_restriction_reasons, ',');
    let mut restriction_add_platforms: Vec<&str> =
        full_split(&options.restriction_add_platforms, ',');
    // TD_ANDROID -> "android", TD_WINDOWS -> "ms", TD_DARWIN -> "ios";
    // the Rust client builds on the POSIX default.
    let mut platform = "";
    if options.ignore_platform_restrictions {
        platform = "";
        restriction_add_platforms.clear();
    }

    if !platform.is_empty() {
        for restriction_reason in restriction_reasons {
            if restriction_reason.platform == platform
                && !ignored_restriction_reasons.contains(&restriction_reason.reason.as_str())
                && restriction_reason.is_sensitive() == sensitive
            {
                return Some(restriction_reason);
            }
        }
    }

    if !restriction_add_platforms.is_empty() {
        for restriction_reason in restriction_reasons {
            if restriction_add_platforms.contains(&restriction_reason.platform.as_str())
                && !ignored_restriction_reasons.contains(&restriction_reason.reason.as_str())
                && restriction_reason.is_sensitive() == sensitive
            {
                return Some(restriction_reason);
            }
        }
    }

    restriction_reasons.iter().find(|restriction_reason| {
        restriction_reason.platform == "all"
            && !ignored_restriction_reasons.contains(&restriction_reason.reason.as_str())
            && restriction_reason.is_sensitive() == sensitive
    })
}

/// `td::get_restriction_info_object`.
pub fn get_restriction_info_object(
    restriction_reasons: &[RestrictionReason],
    options: &RestrictionOptions,
) -> Option<td_api::RestrictionInfo> {
    let has_sensitive_content =
        get_restriction_reason(restriction_reasons, options, true).is_some();
    let restriction_reason = get_restriction_reason(restriction_reasons, options, false);
    let restriction_reason = match restriction_reason {
        None => {
            if has_sensitive_content {
                return Some(td_api::RestrictionInfo {
                    restriction_reason: String::new(),
                    has_sensitive_content,
                });
            }
            return None;
        }
        Some(reason) => reason,
    };
    Some(td_api::RestrictionInfo {
        restriction_reason: restriction_reason.description.clone(),
        has_sensitive_content,
    })
}

/// `td::get_restriction_reasons(Slice)` — the legacy "platforms-type:description"
/// string from old database rows.
pub fn get_restriction_reasons_from_legacy(
    legacy_restriction_reason: &str,
) -> Vec<RestrictionReason> {
    let (type_, description) = split(legacy_restriction_reason, ':');
    let parts = full_split(type_, '-');
    let description = description.trim();

    let mut result = Vec::new();
    if parts.len() <= 1 {
        return result;
    }
    for part in &parts[1..] {
        result.push(RestrictionReason::new(
            (*part).to_owned(),
            parts[0].to_owned(),
            description.to_owned(),
        ));
    }
    result
}

/// `td::get_restriction_reasons(vector<restrictionReason>)`.
pub fn get_restriction_reasons(
    reasons: Vec<telegram_api::RestrictionReason>,
) -> Vec<RestrictionReason> {
    reasons
        .into_iter()
        .map(|reason| RestrictionReason::new(reason.platform, reason.reason, reason.text))
        .collect()
}

/// `td::split(Slice, char)` — the first separator splits; the right side
/// stays the rest (may contain further separators).
fn split(s: &str, separator: char) -> (&str, &str) {
    match s.find(separator) {
        Some(pos) => (&s[..pos], &s[pos + separator.len_utf8()..]),
        None => (s, ""),
    }
}

/// `td::full_split(Slice, char)`.
fn full_split(s: &str, separator: char) -> Vec<&str> {
    s.split(separator).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason(platform: &str, reason_: &str, description: &str) -> RestrictionReason {
        RestrictionReason::new(
            platform.to_owned(),
            reason_.to_owned(),
            description.to_owned(),
        )
    }

    #[test]
    fn empty_description_falls_back_to_the_reason() {
        let r = reason("all", "violence", "");
        assert_eq!(r.description, "violence");
    }

    #[test]
    fn lookup_prefers_current_platform_then_added_then_all() {
        let options = RestrictionOptions {
            ignored_restriction_reasons: String::new(),
            restriction_add_platforms: "web,ms".to_owned(),
            ignore_platform_restrictions: false,
        };
        // The POSIX build has no current platform: "all" wins.
        let reasons = vec![
            reason("ios", "violence", "iOS description"),
            reason("all", "violence", "all description"),
        ];
        assert_eq!(
            get_restriction_reason(&reasons, &options, false)
                .unwrap()
                .description,
            "all description"
        );

        // Ignored reasons are skipped.
        let options = RestrictionOptions {
            ignored_restriction_reasons: "violence".to_owned(),
            restriction_add_platforms: String::new(),
            ignore_platform_restrictions: false,
        };
        assert_eq!(get_restriction_reason(&reasons, &options, false), None);

        // sensitive matches only the sensitive entry.
        let reasons = vec![reason("all", "sensitive", "adult content")];
        assert_eq!(
            get_restriction_reason(&reasons, &options, true)
                .unwrap()
                .description,
            "adult content"
        );
    }

    #[test]
    fn info_object_maps_the_lookup() {
        let options = RestrictionOptions {
            ignored_restriction_reasons: String::new(),
            restriction_add_platforms: String::new(),
            ignore_platform_restrictions: false,
        };
        assert_eq!(get_restriction_info_object(&[], &options), None);
        let reasons = vec![reason("all", "sensitive", "18+")];
        let info = get_restriction_info_object(&reasons, &options).unwrap();
        assert_eq!(
            (info.restriction_reason.as_str(), info.has_sensitive_content),
            ("", true)
        );
        let reasons = vec![reason("all", "violence", "graphic")];
        let info = get_restriction_info_object(&reasons, &options).unwrap();
        assert_eq!(
            (info.restriction_reason.as_str(), info.has_sensitive_content),
            ("graphic", false)
        );
    }

    #[test]
    fn legacy_string_is_expanded_per_platform() {
        // `get_restriction_reasons(Slice)`: the part before the first ':'
        // is '-'-split — parts[0] becomes the REASON, the rest the
        // platforms; the remainder (trimmed) is the description.
        let reasons = get_restriction_reasons_from_legacy("ios-android-all:violence: Bad thing ");
        assert_eq!(reasons.len(), 2);
        assert_eq!(reasons[0].platform, "android");
        assert_eq!(reasons[0].reason, "ios");
        assert_eq!(reasons[0].description, "violence: Bad thing");
        assert_eq!(reasons[1].platform, "all");
        // A single platform part yields nothing.
        assert!(get_restriction_reasons_from_legacy("all:violence").is_empty());
    }

    #[test]
    fn serialization_round_trips() {
        let reasons = vec![reason("all", "violence", "graphic"), reason("ios", "", "")];
        let mut bytes = Vec::new();
        store_restriction_reasons(&mut bytes, &reasons);
        assert_eq!(
            parse_restriction_reasons(&mut Parser::new(&bytes)).unwrap(),
            reasons
        );
    }
}
