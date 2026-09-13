//! Writes the same binlog scenario as conformance/cpp/binlog_write_sample.cpp
//! so the C++ reader tool can verify the Rust-written file.
//!
//! Usage: cargo run -p tddb --example write_sample_binlog -- <output_path>

use tddb::binlog::Binlog;
use tddb::db_key::DbKey;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: write_sample_binlog <output_path>");
    let _ = tddb::binlog::destroy(&path);

    let mut binlog = Binlog::new();
    binlog
        .init(&path, &mut |_| {}, DbKey::empty(), DbKey::empty())
        .ensure();

    let id_a = binlog.add(1, b"AAAA");
    let id_b = binlog.add(2, &[b'x'; 1000]);
    binlog.rewrite(id_a, 1, b"AAAA_REWRITE");
    binlog.erase(id_b);
    let id_c = binlog.add(3, b"CCCC");

    println!("IDS {id_a} {id_b} {id_c}");
    binlog.sync();
    binlog.close(true).ensure();

    let mut binlog = Binlog::new();
    binlog
        .init(
            &path,
            &mut |e| {
                println!(
                    "REPLAY [event_id:{:016x}][type:{}][flags:{}][size:{}][data:{}]",
                    e.id,
                    e.type_,
                    e.flags,
                    e.get_data().len(),
                    String::from_utf8_lossy(e.get_data())
                );
            },
            DbKey::empty(),
            DbKey::empty(),
        )
        .ensure();
    println!("LAST_EVENT_ID {}", binlog.peek_next_event_id() - 1);
    binlog.close(false).ensure();
}
