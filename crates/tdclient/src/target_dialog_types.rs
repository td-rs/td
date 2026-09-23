//! Port of `td/telegram/TargetDialogTypes.{h,cpp}` — the allowed-target
//! bit mask behind `switch_inline` buttons: which dialog classes a bot
//! may be switched into.

use tdutils::status::Status;

use crate::td_api;
use tdmtproto::telegram_api;

/// `td::TargetDialogTypes` (TargetDialogTypes.h:11). `mask == 0` means
/// "any target" — every reader expands it through `get_full_mask`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TargetDialogTypes {
    mask: i64,
}

impl TargetDialogTypes {
    pub(crate) const USERS_MASK: i64 = 1;
    pub(crate) const BOTS_MASK: i64 = 2;
    const CHATS_MASK: i64 = 4;
    const BROADCASTS_MASK: i64 = 8;
    const FULL_MASK: i64 =
        Self::USERS_MASK | Self::BOTS_MASK | Self::CHATS_MASK | Self::BROADCASTS_MASK;

    /// `explicit TargetDialogTypes(int64 mask)` (TargetDialogTypes.h:23).
    pub fn from_mask(mask: i64) -> TargetDialogTypes {
        TargetDialogTypes { mask }
    }

    /// `explicit TargetDialogTypes(const vector<telegram_api::object_ptr<
    /// telegram_api::InlineQueryPeerType>> &)` (TargetDialogTypes.cpp:9-31).
    pub fn from_telegram(peer_types: &[telegram_api::InlineQueryPeerType]) -> TargetDialogTypes {
        let mut mask = 0;
        for peer_type in peer_types {
            match peer_type {
                telegram_api::InlineQueryPeerType::inlineQueryPeerTypePM(_) => {
                    mask |= Self::USERS_MASK;
                }
                telegram_api::InlineQueryPeerType::inlineQueryPeerTypeBotPM(_) => {
                    mask |= Self::BOTS_MASK;
                }
                telegram_api::InlineQueryPeerType::inlineQueryPeerTypeChat(_)
                | telegram_api::InlineQueryPeerType::inlineQueryPeerTypeMegagroup(_) => {
                    mask |= Self::CHATS_MASK;
                }
                telegram_api::InlineQueryPeerType::inlineQueryPeerTypeBroadcast(_) => {
                    mask |= Self::BROADCASTS_MASK;
                }
                // inlineQueryPeerTypeSameBotPM and every future peer type
                // are dropped with the C++ LOG(ERROR).
                peer_type => {
                    tdutils::log_error!("Receive unsupported InlineQueryPeerType {:?}", peer_type);
                }
            }
        }
        TargetDialogTypes { mask }
    }

    /// `TargetDialogTypes::get_target_dialog_types`
    /// (TargetDialogTypes.cpp:33-51) — the null/empty td_api
    /// `targetChatTypes` is a 400.
    pub fn get_target_dialog_types(
        types: Option<&td_api::TargetChatTypes>,
    ) -> Result<TargetDialogTypes, Status> {
        let mut mask = 0;
        if let Some(types) = types {
            if types.allow_user_chats {
                mask |= Self::USERS_MASK;
            }
            if types.allow_bot_chats {
                mask |= Self::BOTS_MASK;
            }
            if types.allow_group_chats {
                mask |= Self::CHATS_MASK;
            }
            if types.allow_channel_chats {
                mask |= Self::BROADCASTS_MASK;
            }
        }
        if mask == 0 {
            return Err(Status::error(400, "At least one chat type must be allowed"));
        }
        Ok(TargetDialogTypes { mask })
    }

    /// `get_mask` (TargetDialogTypes.h:30-32).
    pub fn get_mask(&self) -> i64 {
        self.mask
    }

    /// `get_full_mask` (TargetDialogTypes.h:34-36).
    pub fn get_full_mask(&self) -> i64 {
        if self.mask == 0 {
            Self::FULL_MASK
        } else {
            self.mask
        }
    }

    /// `get_input_peer_types` (TargetDialogTypes.cpp:53-71): the full
    /// mask serializes as the empty vector (the server-side "any" form),
    /// groups expand to both the Chat and Megagroup wire types.
    pub fn get_input_peer_types(&self) -> Vec<telegram_api::InlineQueryPeerType> {
        let mut peer_types = Vec::new();
        if self.mask == Self::FULL_MASK {
            return peer_types;
        }
        if self.mask & Self::USERS_MASK != 0 {
            peer_types.push(telegram_api::InlineQueryPeerType::inlineQueryPeerTypePM(
                telegram_api::inlineQueryPeerTypePM {},
            ));
        }
        if self.mask & Self::BOTS_MASK != 0 {
            peer_types.push(telegram_api::InlineQueryPeerType::inlineQueryPeerTypeBotPM(
                telegram_api::inlineQueryPeerTypeBotPM {},
            ));
        }
        if self.mask & Self::CHATS_MASK != 0 {
            peer_types.push(telegram_api::InlineQueryPeerType::inlineQueryPeerTypeChat(
                telegram_api::inlineQueryPeerTypeChat {},
            ));
            peer_types.push(
                telegram_api::InlineQueryPeerType::inlineQueryPeerTypeMegagroup(
                    telegram_api::inlineQueryPeerTypeMegagroup {},
                ),
            );
        }
        if self.mask & Self::BROADCASTS_MASK != 0 {
            peer_types.push(
                telegram_api::InlineQueryPeerType::inlineQueryPeerTypeBroadcast(
                    telegram_api::inlineQueryPeerTypeBroadcast {},
                ),
            );
        }
        peer_types
    }

    /// `get_target_chat_types_object` (TargetDialogTypes.cpp:73-78).
    pub fn get_target_chat_types_object(&self) -> td_api::TargetChatTypes {
        let mask = self.get_full_mask();
        td_api::TargetChatTypes {
            allow_user_chats: mask & Self::USERS_MASK != 0,
            allow_bot_chats: mask & Self::BOTS_MASK != 0,
            allow_group_chats: mask & Self::CHATS_MASK != 0,
            allow_channel_chats: mask & Self::BROADCASTS_MASK != 0,
        }
    }

    /// `operator<<` (TargetDialogTypes.cpp:80-96) — the expanded mask
    /// rendered as a "(users)(bots)..." chain.
    pub fn print(&self, out: &mut String) {
        let mask = self.get_full_mask();
        if mask & Self::USERS_MASK != 0 {
            out.push_str("(users)");
        }
        if mask & Self::BOTS_MASK != 0 {
            out.push_str("(bots)");
        }
        if mask & Self::CHATS_MASK != 0 {
            out.push_str("(groups)");
        }
        if mask & Self::BROADCASTS_MASK != 0 {
            out.push_str("(channels)");
        }
    }
}

impl std::fmt::Display for TargetDialogTypes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = String::new();
        self.print(&mut out);
        f.write_str(&out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn td_api_mask_and_400() {
        let types = TargetDialogTypes::get_target_dialog_types(Some(&td_api::TargetChatTypes {
            allow_user_chats: true,
            allow_bot_chats: false,
            allow_group_chats: true,
            allow_channel_chats: false,
        }))
        .expect("valid types");
        assert_eq!(
            types.get_mask(),
            TargetDialogTypes::USERS_MASK | TargetDialogTypes::CHATS_MASK
        );
        // A zero mask stays zero: "any target".
        assert_eq!(TargetDialogTypes::default().get_mask(), 0);
        assert_eq!(
            TargetDialogTypes::default().get_full_mask(),
            TargetDialogTypes::FULL_MASK
        );

        let error = TargetDialogTypes::get_target_dialog_types(None).expect_err("null types");
        assert_eq!(error.code(), 400);
        assert_eq!(error.message(), "At least one chat type must be allowed");
        let error = TargetDialogTypes::get_target_dialog_types(Some(&td_api::TargetChatTypes {
            allow_user_chats: false,
            allow_bot_chats: false,
            allow_group_chats: false,
            allow_channel_chats: false,
        }))
        .expect_err("empty types");
        assert_eq!(error.code(), 400);
    }

    #[test]
    fn wire_intake_and_output_round_trip() {
        let types = TargetDialogTypes::from_telegram(&[
            telegram_api::InlineQueryPeerType::inlineQueryPeerTypePM(
                telegram_api::inlineQueryPeerTypePM {},
            ),
            telegram_api::InlineQueryPeerType::inlineQueryPeerTypeMegagroup(
                telegram_api::inlineQueryPeerTypeMegagroup {},
            ),
            telegram_api::InlineQueryPeerType::inlineQueryPeerTypeBroadcast(
                telegram_api::inlineQueryPeerTypeBroadcast {},
            ),
            // SameBotPM is unknown to the mask and is dropped with a LOG.
            telegram_api::InlineQueryPeerType::inlineQueryPeerTypeSameBotPM(
                telegram_api::inlineQueryPeerTypeSameBotPM {},
            ),
        ]);
        assert_eq!(
            types.get_mask(),
            TargetDialogTypes::USERS_MASK
                | TargetDialogTypes::CHATS_MASK
                | TargetDialogTypes::BROADCASTS_MASK
        );

        // Groups expand to both wire peer types; the full mask serializes
        // as the empty vector.
        let peer_types = types.get_input_peer_types();
        assert_eq!(peer_types.len(), 4);
        let full = TargetDialogTypes::from_mask(TargetDialogTypes::FULL_MASK);
        assert!(full.get_input_peer_types().is_empty());
        // Mask 0 means "any" and also renders as the empty vector.
        assert!(
            TargetDialogTypes::default()
                .get_input_peer_types()
                .is_empty()
        );
    }

    #[test]
    fn td_api_object_and_display() {
        let types = TargetDialogTypes::from_mask(TargetDialogTypes::BOTS_MASK);
        assert_eq!(
            types.get_target_chat_types_object(),
            td_api::TargetChatTypes {
                allow_user_chats: false,
                allow_bot_chats: true,
                allow_group_chats: false,
                allow_channel_chats: false,
            }
        );
        // The "any" mask expands in the object builder but not in the mask.
        assert_eq!(
            TargetDialogTypes::default().get_target_chat_types_object(),
            td_api::TargetChatTypes {
                allow_user_chats: true,
                allow_bot_chats: true,
                allow_group_chats: true,
                allow_channel_chats: true,
            }
        );
        assert_eq!(types.to_string(), "(bots)");
        assert_eq!(
            TargetDialogTypes::from_mask(TargetDialogTypes::FULL_MASK).to_string(),
            "(users)(bots)(groups)(channels)"
        );
    }
}
