//! Port of `td/mtproto`: transports, KDF/IGE encryption, auth key, session,
//! multi-DC. The wire-scheme data models with TL binary store are generated
//! at build time from the pinned schemes (S10 telegram_api, S13
//! secret_api + mtproto_api).
//!
//! S53 adds the runtime framing layer: [`message_id`], [`packet_info`],
//! [`auth_key`] and [`auth_data`] (salts, msg_id/seq_no rules, duplicate
//! checkers). S54 adds the stream transports ([`transport`]) with their
//! secret ([`proxy_secret`]) and [`utils`] (hex/base64 helpers). S55 adds
//! the obfuscated transport (AES-CTR header + optional TLS emulation) with
//! the [`tls_reader_byte_flow`].

pub mod auth_data;
pub mod auth_key;
pub mod http_transport;
pub mod message_id;
pub mod packet_info;
pub mod proxy_secret;
pub mod tls_reader_byte_flow;
pub mod transport;
pub mod transport_type;
pub mod utils;

// Generated code keeps C++-identical case-preserved names (`srp_B`, `M1`).
#[allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    clippy::large_enum_variant
)]
pub mod telegram_api {
    include!(concat!(env!("OUT_DIR"), "/telegram_api.rs"));
}

#[allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    clippy::large_enum_variant
)]
pub mod secret_api {
    include!(concat!(env!("OUT_DIR"), "/secret_api.rs"));
}

#[allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    clippy::large_enum_variant
)]
pub mod mtproto_api {
    include!(concat!(env!("OUT_DIR"), "/mtproto_api.rs"));
}

#[cfg(test)]
mod tests {
    use super::{mtproto_api, secret_api, telegram_api};

    #[test]
    fn generated_counts_match_the_c_oracle() {
        // Fixed by the S07 differential dump against td's own tl-parser:
        // 627 types / 1669 ctors / 819 functions. Structs = non-builtin
        // ctors + functions; enums = multi-ctor types + the umbrella.
        assert_eq!(telegram_api::SCHEME_TYPES, 627);
        assert_eq!(telegram_api::SCHEME_CTORS, 1669);
        assert_eq!(telegram_api::SCHEME_FUNCTIONS, 819);
        assert_eq!(telegram_api::GENERATED_STRUCTS, 2479);
        assert_eq!(telegram_api::GENERATED_ENUMS, 245);

        // S13: the secret-chat and MTProto service schemes.
        assert_eq!(secret_api::SCHEME_TYPES, 20);
        assert_eq!(secret_api::SCHEME_CTORS, 94);
        assert_eq!(secret_api::SCHEME_FUNCTIONS, 1);
        assert_eq!(secret_api::GENERATED_STRUCTS, 86);
        assert_eq!(secret_api::GENERATED_ENUMS, 9);

        assert_eq!(mtproto_api::SCHEME_TYPES, 33);
        assert_eq!(mtproto_api::SCHEME_CTORS, 40);
        assert_eq!(mtproto_api::SCHEME_FUNCTIONS, 8);
        assert_eq!(mtproto_api::GENERATED_STRUCTS, 43);
        assert_eq!(mtproto_api::GENERATED_ENUMS, 6);
    }

    #[test]
    fn get_dialogs_store_matches_known_bytes() {
        // messages.getDialogs#a0f4cb4f flags:# exclude_pinned:flags.0?true
        // folder_id:flags.1?int offset_date:int offset_id:int
        // offset_peer:InputPeer limit:int hash:long
        // The stored flags word is var0 = flags | (exclude_pinned << 0), and
        // the folder_id gate reads bit 1 of that captured word — same as the
        // C++ `TlStoreBinary::store((var0 = flags_ | ...), s)`.
        let q = telegram_api::messages_getDialogs {
            flags: 0,
            exclude_pinned: true,
            folder_id: 42,
            offset_date: 0,
            offset_id: 0,
            offset_peer: Box::new(telegram_api::InputPeer::inputPeerEmpty(
                telegram_api::inputPeerEmpty,
            )),
            limit: 0,
            hash: 0,
        };
        let mut out = Vec::new();
        q.store_boxed(&mut out);
        // id + flags word (bit0 set, bit1 clear → no folder_id) +
        // offset_date + offset_id + boxed inputPeerEmpty (id only) +
        // limit + hash(long)
        assert_eq!(out.len(), 4 * 6 + 8);
        assert_eq!(
            i32::from_le_bytes(out[0..4].try_into().unwrap()),
            -1_594_569_905
        );
        assert_eq!(i32::from_le_bytes(out[4..8].try_into().unwrap()), 1);
        assert_eq!(
            i32::from_le_bytes(out[16..20].try_into().unwrap()),
            0x7f3b_18ea
        );

        // With bit 1 set, folder_id is stored after the flags word.
        let q = telegram_api::messages_getDialogs {
            flags: 2,
            exclude_pinned: false,
            folder_id: 42,
            offset_date: 0,
            offset_id: 0,
            offset_peer: Box::new(telegram_api::InputPeer::inputPeerEmpty(
                telegram_api::inputPeerEmpty,
            )),
            limit: 0,
            hash: 0,
        };
        let mut out = Vec::new();
        q.store_boxed(&mut out);
        assert_eq!(out.len(), 4 * 7 + 8);
        assert_eq!(i32::from_le_bytes(out[4..8].try_into().unwrap()), 2);
        assert_eq!(i32::from_le_bytes(out[8..12].try_into().unwrap()), 42);
    }

    #[test]
    fn unit_struct_boxed_store_is_just_the_id() {
        let e = telegram_api::inputPeerEmpty;
        let mut out = Vec::new();
        e.store_boxed(&mut out);
        assert_eq!(out, 0x7f3b_18eai32.to_le_bytes());
    }

    #[test]
    fn vector_count_and_boxed_elements() {
        // messages.getChats#49e9528f id:Vector<long> — a capitalized
        // (boxed) Vector carries the vector#1cb5c415 id before the count
        // (td's `TlStoreBoxed<TlStoreVector, 481674261>`); confirmed
        // byte-for-byte against the C++ oracle in
        // conformance/tl_store_oracle.
        let q = telegram_api::messages_getChats {
            id: vec![1i64, 2, 3],
        };
        let mut out = Vec::new();
        q.store_boxed(&mut out);
        assert_eq!(
            i32::from_le_bytes(out[0..4].try_into().unwrap()),
            0x49e9_528f
        );
        assert_eq!(
            i32::from_le_bytes(out[4..8].try_into().unwrap()),
            0x1cb5_c415
        );
        assert_eq!(i32::from_le_bytes(out[8..12].try_into().unwrap()), 3);
        assert_eq!(i64::from_le_bytes(out[12..20].try_into().unwrap()), 1);
        assert_eq!(i64::from_le_bytes(out[28..36].try_into().unwrap()), 3);
        assert_eq!(out.len(), 4 + 4 + 4 + 3 * 8);
    }
}
