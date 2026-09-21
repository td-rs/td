//! `CallDiscardReason` (td/telegram/CallDiscardReason.h/.cpp) — the M.32
//! Wave-3 call discard reason plane. The two LinkManager invite-link
//! helpers used by the plane are ported here in their minimal form (the
//! full link surface is Wave-2 M.23; `get_t_me_url` keeps the default —
//! the `t_me_url` option gate lands with M.23).

use crate::td_api;
use tdmtproto::telegram_api;

/// `td::CallDiscardReason` — the 6-variant reason + the
/// `UpgradeToGroupCall` invite-link slug.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallDiscardReason {
    pub type_: CallDiscardReasonType,
    pub slug: String,
}

/// `CallDiscardReason::Type` (CallDiscardReason.h:10).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CallDiscardReasonType {
    #[default]
    Empty,
    Missed,
    Disconnected,
    HungUp,
    Declined,
    UpgradeToGroupCall,
}

/// `is_base64url_characters` (misc.cpp) — the slug validation of the
/// invite-link helpers.
fn is_base64url_characters(slug: &str) -> bool {
    slug.bytes()
        .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// `LinkManager::get_group_call_invite_link` (LinkManager.cpp:4299-4307) —
/// the minimal two-branch form; `get_t_me_url` keeps its default.
pub fn get_group_call_invite_link(slug: &str, is_internal: bool) -> String {
    if !is_base64url_characters(slug) {
        return String::new();
    }
    if is_internal {
        format!("tg:call?slug={slug}")
    } else {
        format!("https://t.me/call/{slug}")
    }
}

/// `LinkManager::get_group_call_invite_link_slug`
/// (LinkManager.cpp:4286-4297) — the minimal reverse parse for the
/// `tg:call?slug=` and `t.me/call/` forms (the full `get_link_info`/
/// `parse_url_query` machinery is Wave-2 M.23; this covers the call
/// invite links).
pub fn get_group_call_invite_link_slug(invite_link: &str) -> String {
    let slug = if let Some(slug) = invite_link.strip_prefix("tg:call?slug=") {
        slug
    } else if let Some(slug) = invite_link.strip_prefix("https://t.me/call/") {
        slug
    } else if let Some(slug) = invite_link.strip_prefix("t.me/call/") {
        slug
    } else {
        return String::new();
    };
    if is_base64url_characters(slug) {
        slug.to_string()
    } else {
        String::new()
    }
}

/// `get_call_discard_reason` (CallDiscardReason.cpp:5-25) — the wire
/// `PhoneCallDiscardReason` union.
pub fn get_call_discard_reason(
    reason: &Option<telegram_api::PhoneCallDiscardReason>,
) -> CallDiscardReason {
    let mut result = CallDiscardReason::default();
    match reason {
        Some(telegram_api::PhoneCallDiscardReason::phoneCallDiscardReasonMissed(_)) => {
            result.type_ = CallDiscardReasonType::Missed;
        }
        Some(telegram_api::PhoneCallDiscardReason::phoneCallDiscardReasonDisconnect(_)) => {
            result.type_ = CallDiscardReasonType::Disconnected;
        }
        Some(telegram_api::PhoneCallDiscardReason::phoneCallDiscardReasonHangup(_)) => {
            result.type_ = CallDiscardReasonType::HungUp;
        }
        Some(telegram_api::PhoneCallDiscardReason::phoneCallDiscardReasonBusy(_)) => {
            result.type_ = CallDiscardReasonType::Declined;
        }
        Some(
            telegram_api::PhoneCallDiscardReason::phoneCallDiscardReasonMigrateConferenceCall(
                reason,
            ),
        ) => {
            result.type_ = CallDiscardReasonType::UpgradeToGroupCall;
            result.slug = reason.slug.clone();
        }
        None => {}
    }
    result
}

/// `get_input_phone_call_discard_reason` (CallDiscardReason.cpp:27-42) —
/// the wire form; only `Empty` has no representation.
pub fn get_input_phone_call_discard_reason(
    reason: &CallDiscardReason,
) -> Option<telegram_api::PhoneCallDiscardReason> {
    match reason.type_ {
        CallDiscardReasonType::Empty => None,
        CallDiscardReasonType::Missed => Some(
            telegram_api::PhoneCallDiscardReason::phoneCallDiscardReasonMissed(
                telegram_api::phoneCallDiscardReasonMissed,
            ),
        ),
        CallDiscardReasonType::Disconnected => Some(
            telegram_api::PhoneCallDiscardReason::phoneCallDiscardReasonDisconnect(
                telegram_api::phoneCallDiscardReasonDisconnect,
            ),
        ),
        CallDiscardReasonType::HungUp => Some(
            telegram_api::PhoneCallDiscardReason::phoneCallDiscardReasonHangup(
                telegram_api::phoneCallDiscardReasonHangup,
            ),
        ),
        CallDiscardReasonType::Declined => Some(
            telegram_api::PhoneCallDiscardReason::phoneCallDiscardReasonBusy(
                telegram_api::phoneCallDiscardReasonBusy,
            ),
        ),
        CallDiscardReasonType::UpgradeToGroupCall => Some(
            telegram_api::PhoneCallDiscardReason::phoneCallDiscardReasonMigrateConferenceCall(
                telegram_api::phoneCallDiscardReasonMigrateConferenceCall {
                    slug: reason.slug.clone(),
                },
            ),
        ),
    }
}

/// `get_call_discard_reason_object` (CallDiscardReason.cpp:44-60) — the
/// td_api form.
pub fn get_call_discard_reason_object(reason: &CallDiscardReason) -> td_api::CallDiscardReason {
    match reason.type_ {
        CallDiscardReasonType::Empty => {
            td_api::CallDiscardReason::callDiscardReasonEmpty(td_api::callDiscardReasonEmpty)
        }
        CallDiscardReasonType::Missed => {
            td_api::CallDiscardReason::callDiscardReasonMissed(td_api::callDiscardReasonMissed)
        }
        CallDiscardReasonType::Disconnected => {
            td_api::CallDiscardReason::callDiscardReasonDisconnected(
                td_api::callDiscardReasonDisconnected,
            )
        }
        CallDiscardReasonType::HungUp => {
            td_api::CallDiscardReason::callDiscardReasonHungUp(td_api::callDiscardReasonHungUp)
        }
        CallDiscardReasonType::Declined => {
            td_api::CallDiscardReason::callDiscardReasonDeclined(td_api::callDiscardReasonDeclined)
        }
        CallDiscardReasonType::UpgradeToGroupCall => {
            td_api::CallDiscardReason::callDiscardReasonUpgradeToGroupCall(
                td_api::callDiscardReasonUpgradeToGroupCall {
                    invite_link: get_group_call_invite_link(&reason.slug, false),
                },
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_reasons_map_to_the_local_types() {
        let reason = get_call_discard_reason(&Some(
            telegram_api::PhoneCallDiscardReason::phoneCallDiscardReasonBusy(
                telegram_api::phoneCallDiscardReasonBusy,
            ),
        ));
        assert_eq!(reason.type_, CallDiscardReasonType::Declined);

        let reason = get_call_discard_reason(&Some(
            telegram_api::PhoneCallDiscardReason::phoneCallDiscardReasonMigrateConferenceCall(
                telegram_api::phoneCallDiscardReasonMigrateConferenceCall {
                    slug: "abc-def_123".to_string(),
                },
            ),
        ));
        assert_eq!(reason.type_, CallDiscardReasonType::UpgradeToGroupCall);
        assert_eq!(reason.slug, "abc-def_123");

        let reason = get_call_discard_reason(&None);
        assert_eq!(reason.type_, CallDiscardReasonType::Empty);
    }

    #[test]
    fn input_reasons_map_back_to_the_wire() {
        let missed = CallDiscardReason {
            type_: CallDiscardReasonType::Missed,
            slug: String::new(),
        };
        assert!(matches!(
            get_input_phone_call_discard_reason(&missed),
            Some(telegram_api::PhoneCallDiscardReason::phoneCallDiscardReasonMissed(_))
        ));
        let disconnected = CallDiscardReason {
            type_: CallDiscardReasonType::Disconnected,
            slug: String::new(),
        };
        assert!(matches!(
            get_input_phone_call_discard_reason(&disconnected),
            Some(telegram_api::PhoneCallDiscardReason::phoneCallDiscardReasonDisconnect(_))
        ));
        let upgrade = CallDiscardReason {
            type_: CallDiscardReasonType::UpgradeToGroupCall,
            slug: "abc".to_string(),
        };
        assert!(matches!(
            get_input_phone_call_discard_reason(&upgrade),
            Some(
                telegram_api::PhoneCallDiscardReason::phoneCallDiscardReasonMigrateConferenceCall(
                    _
                )
            )
        ));
        let empty = CallDiscardReason::default();
        assert!(get_input_phone_call_discard_reason(&empty).is_none());
    }

    #[test]
    fn object_reasons_carry_the_upgrade_link() {
        let upgrade = CallDiscardReason {
            type_: CallDiscardReasonType::UpgradeToGroupCall,
            slug: "abc".to_string(),
        };
        match get_call_discard_reason_object(&upgrade) {
            td_api::CallDiscardReason::callDiscardReasonUpgradeToGroupCall(object) => {
                assert_eq!(object.invite_link, "https://t.me/call/abc");
            }
            _ => panic!("expected the upgrade reason"),
        }
        // an invalid slug produces an empty link (LinkManager.cpp:4300-4302)
        let bad = CallDiscardReason {
            type_: CallDiscardReasonType::UpgradeToGroupCall,
            slug: "not a slug".to_string(),
        };
        match get_call_discard_reason_object(&bad) {
            td_api::CallDiscardReason::callDiscardReasonUpgradeToGroupCall(object) => {
                assert_eq!(object.invite_link, "");
            }
            _ => panic!("expected the upgrade reason"),
        }
    }

    #[test]
    fn invite_link_helpers_match_the_cpp_shapes() {
        assert_eq!(
            get_group_call_invite_link("abc-123", false),
            "https://t.me/call/abc-123"
        );
        assert_eq!(
            get_group_call_invite_link("abc-123", true),
            "tg:call?slug=abc-123"
        );
        assert_eq!(get_group_call_invite_link("a b", false), "");
    }
}
