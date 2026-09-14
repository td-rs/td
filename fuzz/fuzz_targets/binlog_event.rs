//! Fuzz target: the binlog event reader — `BinlogEvent::init` over
//! arbitrary bytes (the parse half of the S40 byte-identical format).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = tddb::binlog::event::BinlogEvent::init(data.to_vec());
});
