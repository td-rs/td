//! Port of `td/mtproto/TransportType.h`.

use crate::proxy_secret::ProxySecret;

/// `td::mtproto::TransportType::Type`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum TransportTypeEnum {
    #[default]
    Tcp,
    ObfuscatedTcp,
    Http,
}

/// `td::mtproto::TransportType`.
#[derive(Debug, Default, Clone)]
pub struct TransportType {
    pub type_: TransportTypeEnum,
    pub dc_id: i16,
    pub secret: ProxySecret,
}

impl TransportType {
    pub fn new(type_: TransportTypeEnum, dc_id: i16, secret: ProxySecret) -> Self {
        Self {
            type_,
            dc_id,
            secret,
        }
    }
}
