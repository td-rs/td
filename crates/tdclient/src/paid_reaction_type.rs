//! Port of `td/telegram/PaidReactionType.{h,cpp,hpp}` (M.16.1): the
//! privacy of a paid reaction — regular (as the current user), anonymous,
//! or sent on behalf of another dialog the user controls. The value plane
//! (including the byte-exact store/parse the default-type pmc record
//! uses) is complete; the manager-mediated constructors and builders go
//! through [`PaidReactionTypeEnv`], whose surfaces live with the dialog
//! plane on the lines that own it (M.6/M.10 integration).

use crate::dialog_id::DialogId;
use crate::store_parse::{Parser, store_flags, store_i32};
use crate::td_api;
use tdmtproto::telegram_api;

/// `PaidReactionType::Type` (PaidReactionType.h:22) — the stored
/// discriminant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Type {
    #[default]
    Regular,
    Anonymous,
    Dialog,
}

impl Type {
    fn as_i32(self) -> i32 {
        match self {
            Type::Regular => 0,
            Type::Anonymous => 1,
            Type::Dialog => 2,
        }
    }

    fn from_i32(value: i32) -> Option<Type> {
        match value {
            0 => Some(Type::Regular),
            1 => Some(Type::Anonymous),
            2 => Some(Type::Dialog),
            _ => None,
        }
    }
}

/// The `Td *td` slice the C++ constructor/builders read. The C++
/// `AccessRights::Write` argument of `check_dialog_access` is the only
/// rights level ever passed here, so it is implicit in the port.
pub trait PaidReactionTypeEnv {
    /// `dialog_manager_->have_dialog_info`.
    fn have_dialog_info(&self, dialog_id: DialogId) -> bool;
    /// `dialog_manager_->force_create_dialog(dialog_id, "PaidReactionType")`.
    fn force_create_dialog(&mut self, dialog_id: DialogId);
    /// `dialog_manager_->check_dialog_access(dialog_id, false, Write, ...)`.
    fn check_dialog_access(
        &self,
        dialog_id: DialogId,
        source: &str,
    ) -> Result<(), tdutils::status::Status>;
    /// `dialog_manager_->is_broadcast_channel`.
    fn is_broadcast_channel(&self, dialog_id: DialogId) -> bool;
    /// `dialog_manager_->get_input_peer(dialog_id, AccessRights::Write)`.
    fn get_input_peer(&self, dialog_id: DialogId) -> Option<telegram_api::InputPeer>;
    /// `dialog_manager_->get_chat_id_object(dialog_id, source)`.
    fn get_chat_id_object(&self, dialog_id: DialogId, source: &str) -> i64;
}

/// `PaidReactionType` (PaidReactionType.h).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaidReactionType {
    type_: Type,
    dialog_id: DialogId,
}

impl PaidReactionType {
    /// `PaidReactionType(Td *, const telegram_api::object_ptr<
    /// PaidReactionPrivacy> &)` (PaidReactionType.cpp:19-44).
    pub fn from_telegram(
        env: &mut impl PaidReactionTypeEnv,
        type_: &telegram_api::PaidReactionPrivacy,
    ) -> PaidReactionType {
        let mut result = PaidReactionType::default();
        match type_ {
            telegram_api::PaidReactionPrivacy::paidReactionPrivacyDefault(_) => {}
            telegram_api::PaidReactionPrivacy::paidReactionPrivacyAnonymous(_) => {
                result.type_ = Type::Anonymous;
            }
            telegram_api::PaidReactionPrivacy::paidReactionPrivacyPeer(type_) => {
                // the InputDialogId peer fold (InputDialogId.cpp:29-66):
                // user/chat/channel peers map to their dialog ids, every
                // other peer form stays invalid and is rejected below
                let dialog_id = match type_.peer.as_deref() {
                    Some(telegram_api::InputPeer::inputPeerUser(peer)) => {
                        DialogId::from_user_id(crate::user_manager::UserId::new(peer.user_id))
                    }
                    Some(telegram_api::InputPeer::inputPeerChat(peer)) => {
                        DialogId::from_chat_id(crate::chat_manager::ChatId::new(peer.chat_id))
                    }
                    Some(telegram_api::InputPeer::inputPeerChannel(peer)) => {
                        DialogId::from_channel_id(crate::chat_manager::ChannelId::new(
                            peer.channel_id,
                        ))
                    }
                    _ => DialogId::new(0),
                };
                if env.have_dialog_info(dialog_id) {
                    env.force_create_dialog(dialog_id);
                    result.type_ = Type::Dialog;
                    result.dialog_id = dialog_id;
                } else {
                    tdutils::log_error!("Receive paid reaction type {}", dialog_id.get());
                }
            }
        }
        result
    }

    /// `PaidReactionType(Td *, const td_api::object_ptr<PaidReactionType> &)`
    /// (PaidReactionType.cpp:46-71).
    pub fn from_td_api(
        env: &impl PaidReactionTypeEnv,
        type_: Option<td_api::PaidReactionType>,
    ) -> PaidReactionType {
        let mut result = PaidReactionType::default();
        let Some(type_) = type_ else {
            return result;
        };
        match type_ {
            td_api::PaidReactionType::paidReactionTypeRegular(_) => {}
            td_api::PaidReactionType::paidReactionTypeAnonymous(_) => {
                result.type_ = Type::Anonymous;
            }
            td_api::PaidReactionType::paidReactionTypeChat(type_) => {
                result.type_ = Type::Dialog;
                let dialog_id = DialogId::new(type_.chat_id);
                let is_accessible = env
                    .check_dialog_access(dialog_id, "PaidReactionType")
                    .is_ok();
                if !is_accessible || !env.is_broadcast_channel(dialog_id) {
                    return result;
                }
                result.dialog_id = dialog_id;
            }
        }
        result
    }

    /// `PaidReactionType::legacy(bool is_anonymous)` (PaidReactionType.cpp:73-79).
    pub fn legacy(is_anonymous: bool) -> PaidReactionType {
        let mut result = PaidReactionType::default();
        if is_anonymous {
            result.type_ = Type::Anonymous;
        }
        result
    }

    /// `PaidReactionType::dialog(DialogId)` (PaidReactionType.cpp:81-86).
    pub fn dialog(dialog_id: DialogId) -> PaidReactionType {
        PaidReactionType {
            type_: Type::Dialog,
            dialog_id,
        }
    }

    /// `get_input_paid_reaction_privacy` (PaidReactionType.cpp:88-106).
    pub fn get_input_paid_reaction_privacy(
        &self,
        env: &impl PaidReactionTypeEnv,
    ) -> telegram_api::PaidReactionPrivacy {
        match self.type_ {
            Type::Regular => telegram_api::PaidReactionPrivacy::paidReactionPrivacyDefault(
                telegram_api::paidReactionPrivacyDefault {},
            ),
            Type::Anonymous => telegram_api::PaidReactionPrivacy::paidReactionPrivacyAnonymous(
                telegram_api::paidReactionPrivacyAnonymous {},
            ),
            Type::Dialog => {
                let Some(input_peer) = env.get_input_peer(self.dialog_id) else {
                    return telegram_api::PaidReactionPrivacy::paidReactionPrivacyAnonymous(
                        telegram_api::paidReactionPrivacyAnonymous {},
                    );
                };
                telegram_api::PaidReactionPrivacy::paidReactionPrivacyPeer(
                    telegram_api::paidReactionPrivacyPeer {
                        peer: Some(Box::new(input_peer)),
                    },
                )
            }
        }
    }

    /// `get_paid_reaction_type_object` (PaidReactionType.cpp:108-121).
    pub fn get_paid_reaction_type_object(
        &self,
        env: &impl PaidReactionTypeEnv,
    ) -> td_api::PaidReactionType {
        match self.type_ {
            Type::Regular => td_api::PaidReactionType::paidReactionTypeRegular(
                td_api::paidReactionTypeRegular {},
            ),
            Type::Anonymous => td_api::PaidReactionType::paidReactionTypeAnonymous(
                td_api::paidReactionTypeAnonymous {},
            ),
            Type::Dialog => {
                td_api::PaidReactionType::paidReactionTypeChat(td_api::paidReactionTypeChat {
                    chat_id: env
                        .get_chat_id_object(self.dialog_id, "get_paid_reaction_type_object"),
                })
            }
        }
    }

    /// `get_update_default_paid_reaction_type` (PaidReactionType.cpp:123-126).
    pub fn get_update_default_paid_reaction_type(
        &self,
        env: &impl PaidReactionTypeEnv,
    ) -> td_api::Update {
        td_api::Update::updateDefaultPaidReactionType(td_api::updateDefaultPaidReactionType {
            r#type: Some(self.get_paid_reaction_type_object(env)),
        })
    }

    /// `is_valid` (PaidReactionType.h:47-49).
    pub fn is_valid(&self) -> bool {
        match self.type_ {
            Type::Dialog => self.dialog_id.is_valid(),
            _ => true,
        }
    }

    /// `get_dialog_id` (PaidReactionType.cpp:128-140).
    pub fn get_dialog_id(&self, my_dialog_id: DialogId) -> DialogId {
        match self.type_ {
            Type::Regular => my_dialog_id,
            Type::Anonymous => DialogId::new(0),
            Type::Dialog => self.dialog_id,
        }
    }

    /// `PaidReactionType::store` (PaidReactionType.hpp).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_dialog_id = self.dialog_id.is_valid();
        store_flags(out, [has_dialog_id]);
        store_i32(out, self.type_.as_i32());
        if has_dialog_id {
            self.dialog_id.store(out);
        }
    }

    /// `PaidReactionType::parse` (PaidReactionType.hpp).
    pub fn parse(parser: &mut Parser<'_>) -> Result<PaidReactionType, &'static str> {
        let mut result = PaidReactionType::default();
        let [has_dialog_id] = parser.parse_flags()?;
        result.type_ = Type::from_i32(parser.i32_()?).ok_or("Invalid PaidReactionType")?;
        if has_dialog_id {
            result.dialog_id = DialogId::parse_from(parser)?;
        }
        Ok(result)
    }
}

impl std::fmt::Display for PaidReactionType {
    /// `operator<<` (PaidReactionType.cpp:150-162).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.type_ {
            Type::Regular => write!(f, "non-anonymous paid reaction"),
            Type::Anonymous => write!(f, "anonymous paid reaction"),
            Type::Dialog => write!(f, "paid reaction via {}", self.dialog_id.get()),
        }
    }
}

#[cfg(test)]
mod paid_reaction_type_tests {
    use super::*;
    use crate::store_parse::store_log_event_version;

    #[test]
    fn value_plane_and_store_parse_round_trip() {
        assert_eq!(PaidReactionType::default(), PaidReactionType::legacy(false));
        assert_eq!(
            PaidReactionType::legacy(true),
            PaidReactionType {
                type_: Type::Anonymous,
                dialog_id: DialogId::new(0),
            }
        );

        let dialog_type = PaidReactionType::dialog(DialogId::new(-1001234567890));
        assert!(dialog_type.is_valid());
        assert_eq!(
            dialog_type.get_dialog_id(DialogId::new(-99)),
            DialogId::new(-1001234567890)
        );
        assert_eq!(
            PaidReactionType::legacy(false).get_dialog_id(DialogId::new(-99)),
            DialogId::new(-99)
        );
        assert_eq!(
            PaidReactionType::legacy(true).get_dialog_id(DialogId::new(-99)),
            DialogId::new(0)
        );

        // an invalid dialog id invalidates the Dialog form only
        let invalid = PaidReactionType::dialog(DialogId::new(0));
        assert!(!invalid.is_valid());

        for type_ in [
            PaidReactionType::default(),
            PaidReactionType::legacy(true),
            dialog_type,
        ] {
            let mut out = Vec::new();
            store_log_event_version(&mut out);
            type_.store(&mut out);
            let mut parser = Parser::new_log_event(&out).unwrap();
            let parsed = PaidReactionType::parse(&mut parser).unwrap();
            parser.fetch_end().unwrap();
            assert_eq!(parsed, type_);
        }
    }
}
