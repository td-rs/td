//! `CallProtocol`, `CallConnection`, `CallState` (td/telegram/CallActor.h +
//! CallActor.cpp) — the M.32 Wave-3 call protocol/state value plane.

use crate::call_discard_reason::{CallDiscardReason, get_call_discard_reason_object};
use crate::td_api;
use tdmtproto::telegram_api;

/// `td::CallProtocol` (CallActor.h:31-51) — the protocol agreement between
/// the two call endpoints.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CallProtocol {
    pub udp_p2p: bool,
    pub udp_reflector: bool,
    pub min_layer: i32,
    pub max_layer: i32,
    pub library_versions: Vec<String>,
}

impl CallProtocol {
    /// `CallProtocol(const telegram_api::phoneCallProtocol &)`
    /// (CallActor.cpp:70-76).
    pub fn from_telegram(protocol: &telegram_api::PhoneCallProtocol) -> CallProtocol {
        CallProtocol {
            udp_p2p: protocol.udp_p2p,
            udp_reflector: protocol.udp_reflector,
            min_layer: protocol.min_layer,
            max_layer: protocol.max_layer,
            library_versions: protocol.library_versions.clone(),
        }
    }

    /// `CallProtocol(const td_api::callProtocol &)` (CallActor.cpp:83-89).
    pub fn from_td_api(protocol: &td_api::CallProtocol) -> CallProtocol {
        CallProtocol {
            udp_p2p: protocol.udp_p2p,
            udp_reflector: protocol.udp_reflector,
            min_layer: protocol.min_layer,
            max_layer: protocol.max_layer,
            library_versions: protocol.library_versions.clone(),
        }
    }

    /// `get_input_phone_call_protocol` (CallActor.cpp:78-81).
    pub fn get_input_phone_call_protocol(&self) -> telegram_api::PhoneCallProtocol {
        telegram_api::PhoneCallProtocol {
            flags: 0,
            udp_p2p: self.udp_p2p,
            udp_reflector: self.udp_reflector,
            min_layer: self.min_layer,
            max_layer: self.max_layer,
            library_versions: self.library_versions.clone(),
        }
    }

    /// `get_call_protocol_object` (CallActor.cpp:122-125).
    pub fn get_call_protocol_object(&self) -> td_api::CallProtocol {
        td_api::CallProtocol {
            udp_p2p: self.udp_p2p,
            udp_reflector: self.udp_reflector,
            min_layer: self.min_layer,
            max_layer: self.max_layer,
            library_versions: self.library_versions.clone(),
        }
    }
}

/// `CallConnection::Type` (CallActor.h:56).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallConnectionType {
    Telegram,
    Webrtc,
}

/// `td::CallConnection` (CallActor.h:55-73).
#[derive(Clone, Debug, PartialEq)]
pub struct CallConnection {
    pub type_: CallConnectionType,
    pub id: i64,
    pub ip: String,
    pub ipv6: String,
    pub port: i32,
    // Telegram
    pub peer_tag: Vec<u8>,
    pub is_tcp: bool,
    // WebRTC
    pub username: String,
    pub password: String,
    pub supports_turn: bool,
    pub supports_stun: bool,
}

impl CallConnection {
    /// `CallConnection(const telegram_api::PhoneConnection &)`
    /// (CallActor.cpp:91-120).
    pub fn from_telegram(connection: &telegram_api::PhoneConnection) -> CallConnection {
        match connection {
            telegram_api::PhoneConnection::phoneConnection(connection) => CallConnection {
                type_: CallConnectionType::Telegram,
                id: connection.id,
                ip: connection.ip.clone(),
                ipv6: connection.ipv6.clone(),
                port: connection.port,
                peer_tag: connection.peer_tag.clone(),
                is_tcp: connection.tcp,
                username: String::new(),
                password: String::new(),
                supports_turn: false,
                supports_stun: false,
            },
            telegram_api::PhoneConnection::phoneConnectionWebrtc(connection) => CallConnection {
                type_: CallConnectionType::Webrtc,
                id: connection.id,
                ip: connection.ip.clone(),
                ipv6: connection.ipv6.clone(),
                port: connection.port,
                peer_tag: Vec::new(),
                is_tcp: false,
                username: connection.username.clone(),
                password: connection.password.clone(),
                supports_turn: connection.turn,
                supports_stun: connection.stun,
            },
        }
    }

    /// `get_call_server_object` (CallActor.cpp:127-140).
    pub fn get_call_server_object(&self) -> td_api::CallServer {
        let server_type = match self.type_ {
            CallConnectionType::Telegram => {
                td_api::CallServerType::callServerTypeTelegramReflector(
                    td_api::callServerTypeTelegramReflector {
                        peer_tag: self.peer_tag.clone(),
                        is_tcp: self.is_tcp,
                    },
                )
            }
            CallConnectionType::Webrtc => {
                td_api::CallServerType::callServerTypeWebrtc(td_api::callServerTypeWebrtc {
                    username: self.username.clone(),
                    password: self.password.clone(),
                    supports_turn: self.supports_turn,
                    supports_stun: self.supports_stun,
                })
            }
        };
        td_api::CallServer {
            id: self.id,
            ip_address: self.ip.clone(),
            ipv6_address: self.ipv6.clone(),
            port: self.port,
            r#type: Some(server_type),
        }
    }
}

/// `CallState::Type` (CallActor.h:76).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CallStateType {
    #[default]
    Empty,
    Pending,
    ExchangingKey,
    Ready,
    HangingUp,
    Discarded,
    Error,
}

/// `td::CallState` (CallActor.h:74-99).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CallState {
    pub type_: CallStateType,
    pub protocol: CallProtocol,
    pub connections: Vec<CallConnection>,
    pub discard_reason: CallDiscardReason,
    pub is_created: bool,
    pub is_received: bool,
    pub need_debug_information: bool,
    pub need_rating: bool,
    pub need_log: bool,
    pub key_fingerprint: i64,
    pub key: Vec<u8>,
    pub config: String,
    pub emojis_fingerprint: Vec<String>,
    pub custom_parameters: String,
    pub allow_p2p: bool,
    pub conference_supported: bool,
    pub error: Option<crate::td_api::Error>,
}

impl CallState {
    /// `get_call_state_object` (CallActor.cpp:142-167).
    pub fn get_call_state_object(&self) -> td_api::CallState {
        match self.type_ {
            CallStateType::Pending => {
                td_api::CallState::callStatePending(td_api::callStatePending {
                    is_created: self.is_created,
                    is_received: self.is_received,
                })
            }
            CallStateType::ExchangingKey => {
                td_api::CallState::callStateExchangingKeys(td_api::callStateExchangingKeys)
            }
            CallStateType::Ready => {
                let servers = self
                    .connections
                    .iter()
                    .map(CallConnection::get_call_server_object)
                    .collect();
                td_api::CallState::callStateReady(td_api::callStateReady {
                    protocol: Some(self.protocol.get_call_protocol_object()),
                    servers,
                    config: self.config.clone(),
                    encryption_key: self.key.clone(),
                    emojis: self.emojis_fingerprint.clone(),
                    allow_p2p: self.allow_p2p,
                    is_group_call_supported: self.conference_supported,
                    custom_parameters: self.custom_parameters.clone(),
                })
            }
            CallStateType::HangingUp => {
                td_api::CallState::callStateHangingUp(td_api::callStateHangingUp)
            }
            CallStateType::Discarded => {
                td_api::CallState::callStateDiscarded(td_api::callStateDiscarded {
                    reason: Some(get_call_discard_reason_object(&self.discard_reason)),
                    need_rating: self.need_rating,
                    need_debug_information: self.need_debug_information,
                    need_log: self.need_log,
                })
            }
            CallStateType::Error => {
                let error = self
                    .error
                    .clone()
                    .expect("Error call state carries an error");
                td_api::CallState::callStateError(td_api::callStateError { error: Some(error) })
            }
            CallStateType::Empty => {
                panic!("get_call_state_object on the empty call state")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_round_trips_through_the_wire_and_td_api() {
        let protocol = CallProtocol {
            udp_p2p: true,
            udp_reflector: false,
            min_layer: 65,
            max_layer: 92,
            library_versions: vec!["1.0".to_string(), "2.0".to_string()],
        };
        let wire = protocol.get_input_phone_call_protocol();
        assert_eq!(CallProtocol::from_telegram(&wire), protocol);
        let object = protocol.get_call_protocol_object();
        assert_eq!(CallProtocol::from_td_api(&object), protocol);
    }

    #[test]
    fn connections_map_to_the_server_objects() {
        let telegram = CallConnection::from_telegram(
            &telegram_api::PhoneConnection::phoneConnection(telegram_api::phoneConnection {
                flags: 0,
                tcp: true,
                id: 7,
                ip: "1.2.3.4".to_string(),
                ipv6: "::1".to_string(),
                port: 443,
                peer_tag: b"tag".to_vec(),
            }),
        );
        assert_eq!(telegram.type_, CallConnectionType::Telegram);
        assert!(telegram.is_tcp);
        match telegram.get_call_server_object().r#type {
            Some(td_api::CallServerType::callServerTypeTelegramReflector(server_type)) => {
                assert_eq!(server_type.peer_tag, b"tag");
                assert!(server_type.is_tcp);
            }
            other => panic!("unexpected server type: {other:?}"),
        }

        let webrtc =
            CallConnection::from_telegram(&telegram_api::PhoneConnection::phoneConnectionWebrtc(
                telegram_api::phoneConnectionWebrtc {
                    flags: 0,
                    turn: true,
                    stun: false,
                    id: 8,
                    ip: "5.6.7.8".to_string(),
                    ipv6: String::new(),
                    port: 80,
                    username: "user".to_string(),
                    password: "pass".to_string(),
                },
            ));
        assert_eq!(webrtc.type_, CallConnectionType::Webrtc);
        assert!(webrtc.supports_turn);
        match webrtc.get_call_server_object().r#type {
            Some(td_api::CallServerType::callServerTypeWebrtc(server_type)) => {
                assert_eq!(server_type.username, "user");
                assert!(server_type.supports_turn);
                assert!(!server_type.supports_stun);
            }
            other => panic!("unexpected server type: {other:?}"),
        }
    }

    #[test]
    fn call_state_objects_cover_the_variants() {
        let mut state = CallState {
            type_: CallStateType::Ready,
            ..CallState::default()
        };
        state.connections.push(CallConnection {
            type_: CallConnectionType::Telegram,
            id: 1,
            ip: "1.1.1.1".to_string(),
            ipv6: String::new(),
            port: 443,
            peer_tag: b"tag".to_vec(),
            is_tcp: false,
            username: String::new(),
            password: String::new(),
            supports_turn: false,
            supports_stun: false,
        });
        state.key = vec![1, 2, 3];
        state.emojis_fingerprint = vec!["😀".to_string()];
        match state.get_call_state_object() {
            td_api::CallState::callStateReady(object) => {
                assert_eq!(object.servers.len(), 1);
                assert_eq!(object.encryption_key, vec![1, 2, 3]);
                assert_eq!(object.emojis, vec!["😀"]);
            }
            other => panic!("unexpected state object: {other:?}"),
        }

        assert!(matches!(
            CallState {
                type_: CallStateType::Pending,
                ..CallState::default()
            }
            .get_call_state_object(),
            td_api::CallState::callStatePending(_)
        ));
        assert!(matches!(
            CallState {
                type_: CallStateType::ExchangingKey,
                ..CallState::default()
            }
            .get_call_state_object(),
            td_api::CallState::callStateExchangingKeys(_)
        ));
        assert!(matches!(
            CallState {
                type_: CallStateType::Discarded,
                ..CallState::default()
            }
            .get_call_state_object(),
            td_api::CallState::callStateDiscarded(_)
        ));
        assert!(matches!(
            CallState {
                type_: CallStateType::Error,
                error: Some(crate::td_api::Error {
                    code: 400,
                    message: "err".to_string(),
                }),
                ..CallState::default()
            }
            .get_call_state_object(),
            td_api::CallState::callStateError(_)
        ));
    }
}
