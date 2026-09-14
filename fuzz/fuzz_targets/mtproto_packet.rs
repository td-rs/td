//! Fuzz target: the encrypted MTProto packet parser — arbitrary server
//! bytes through `packet::read_with_x` under a fixed dummy auth key (the
//! decrypt + seq_no/msg_id + body-parse path; a wrong key surfaces as an
//! error status, which is exactly the non-panic contract under test).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let auth_key = tdmtproto::auth_key::AuthKey::new(0, vec![7u8; 256]);
    let mut packet_info = tdmtproto::packet_info::PacketInfo::new();
    let mut message = data.to_vec();
    let _ = tdmtproto::packet::read_with_x(&mut message, 0, &auth_key, &mut packet_info, 8);
});
