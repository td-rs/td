//! `CallId` + `InputCallId` (td/telegram/CallId.h,
//! td/telegram/InputCallId.{h,cpp}) — the M.32 Wave-3 call id plane.

use crate::chat_manager::MessageId;
use crate::dialog_id::{DialogId, MessageFullId};
use crate::td_api;
use tdutils::status::Status;

/// `td::CallId` (CallId.h) — the local call identifier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct CallId(i32);

impl CallId {
    pub fn new(call_id: i32) -> CallId {
        CallId(call_id)
    }

    pub fn is_valid(self) -> bool {
        self.0 != 0
    }

    pub fn get(self) -> i32 {
        self.0
    }

    /// `get_call_id_object` (CallId.h:19-21).
    pub fn get_call_id_object(self) -> td_api::CallId {
        td_api::CallId { id: self.0 }
    }
}

/// `td::InputCallId` (InputCallId.h) — a call referenced by its id or by
/// the message that started it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InputCallId {
    Call(CallId),
    Message(MessageFullId),
}

impl InputCallId {
    /// `get_input_call_id` (InputCallId.cpp:14-46).
    pub fn get_input_call_id(
        input_call: &Option<td_api::InputCall>,
    ) -> std::result::Result<InputCallId, Status> {
        let Some(input_call) = input_call else {
            return Err(Status::error(400, "Input call must be non-empty"));
        };
        match input_call {
            td_api::InputCall::inputCallDiscarded(call) => {
                let call_id = CallId::new(call.call_id);
                if !call_id.is_valid() {
                    return Err(Status::error(400, "Invalid call identifier specified"));
                }
                Ok(InputCallId::Call(call_id))
            }
            td_api::InputCall::inputCallFromMessage(call) => {
                let dialog_id = DialogId::new(call.chat_id);
                let message_id = MessageId::new(call.message_id);
                if !dialog_id.is_valid() {
                    return Err(Status::error(400, "Invalid chat identifier specified"));
                }
                if !message_id.is_valid() {
                    return Err(Status::error(400, "Invalid message identifier specified"));
                }
                if !message_id.is_server() {
                    return Err(Status::error(400, "Wrong message identifier specified"));
                }
                Ok(InputCallId::Message(MessageFullId::new(
                    dialog_id, message_id,
                )))
            }
        }
    }

    /// `get_call_id` (InputCallId.h:17-20) — the C++ CHECKs the type.
    pub fn get_call_id(self) -> CallId {
        match self {
            InputCallId::Call(call_id) => call_id,
            InputCallId::Message(_) => {
                panic!("InputCallId::get_call_id on a message input")
            }
        }
    }

    /// `get_message_full_id` (InputCallId.h:21-23).
    pub fn get_message_full_id(self) -> MessageFullId {
        match self {
            InputCallId::Call(_) => panic!("InputCallId::get_message_full_id on a call input"),
            InputCallId::Message(message_full_id) => message_full_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_id_validity_and_object() {
        let call_id = CallId::new(5);
        assert!(call_id.is_valid());
        assert_eq!(call_id.get(), 5);
        assert_eq!(call_id.get_call_id_object().id, 5);
        assert!(!CallId::default().is_valid());
    }

    #[test]
    fn input_call_id_parses_the_variants() {
        let id = InputCallId::get_input_call_id(&Some(td_api::InputCall::inputCallDiscarded(
            td_api::inputCallDiscarded { call_id: 17 },
        )))
        .unwrap();
        assert_eq!(id, InputCallId::Call(CallId::new(17)));

        let id = InputCallId::get_input_call_id(&Some(td_api::InputCall::inputCallFromMessage(
            td_api::inputCallFromMessage {
                chat_id: 3,
                message_id: 10 << 20,
            },
        )))
        .unwrap();
        assert_eq!(
            id,
            InputCallId::Message(MessageFullId::new(
                DialogId::new(3),
                MessageId::new(10 << 20)
            ))
        );

        assert!(InputCallId::get_input_call_id(&None).is_err());
        assert!(
            InputCallId::get_input_call_id(&Some(td_api::InputCall::inputCallDiscarded(
                td_api::inputCallDiscarded { call_id: 0 }
            )))
            .is_err()
        );
        assert!(
            InputCallId::get_input_call_id(&Some(td_api::InputCall::inputCallFromMessage(
                td_api::inputCallFromMessage {
                    chat_id: 0,
                    message_id: 10 << 20,
                }
            )))
            .is_err()
        );
        // a non-server message identifier is rejected (MessageId.h:122-125)
        assert!(
            InputCallId::get_input_call_id(&Some(td_api::InputCall::inputCallFromMessage(
                td_api::inputCallFromMessage {
                    chat_id: 3,
                    message_id: 10,
                }
            )))
            .is_err()
        );
    }
}
