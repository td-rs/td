//! Forensic binlog dump: replay events, decode pmc entries (BinlogPmc 0x4327,
//! ConfigPmc 0x1f18 = [key TL string][value TL string], NO version int),
//! print everything else raw with the payload's first int32.
//!
//! Usage: cargo run -p tddb --example dump_binlog -- <path> [raw-key-ascii]

use tddb::binlog::Binlog;
use tddb::db_key::DbKey;

fn tl_string(data: &[u8], pos: &mut usize) -> Option<(String, Vec<u8>)> {
    if *pos >= data.len() {
        return None;
    }
    let mut p = *pos;
    let len;
    if data[p] == 0xfe && p + 4 <= data.len() {
        len =
            (data[p + 1] as usize) | ((data[p + 2] as usize) << 8) | ((data[p + 3] as usize) << 16);
        p += 4;
    } else {
        len = data[p] as usize;
        p += 1;
    }
    if p + len > data.len() {
        return None;
    }
    let s = String::from_utf8_lossy(&data[p..p + len]).to_string();
    let raw = data[p..p + len].to_vec();
    p += len;
    while !p.is_multiple_of(4) {
        p += 1;
    }
    *pos = p;
    Some((s, raw))
}

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: dump_binlog <path> [raw-key-ascii]");
    let key = match args.next() {
        Some(raw) => DbKey::raw_key(raw.as_bytes().to_vec()),
        None => DbKey::empty(),
    };

    let mut binlog = Binlog::new();
    binlog
        .init(
            &path,
            &mut |e| {
                let data = e.get_data();
                let t = e.type_ & 0x7fff_ffff;
                let header = format!(
                    "id:{:016x} type:{} (0x{:x}) flags:{} size:{}",
                    e.id,
                    e.type_,
                    t,
                    e.flags,
                    data.len()
                );
                if t == 0x4327 || t == 0x1f18 {
                    let mut pos = 0usize;
                    let (key, _) = match tl_string(data, &mut pos) {
                        Some(v) => v,
                        None => {
                            println!("{header} BAD-KEY {}", hex(&data[..data.len().min(32)]));
                            return;
                        }
                    };
                    let (value, raw) = match tl_string(data, &mut pos) {
                        Some(v) => v,
                        None => {
                            println!(
                                "{header} key:{key:?} BAD-VALUE {}",
                                hex(&data[pos..(pos + 16).min(data.len())])
                            );
                            return;
                        }
                    };
                    println!(
                        "{header} key:{key:?} vlen:{} value:{} hexp:{}",
                        raw.len(),
                        value.chars().take(60).collect::<String>(),
                        if std::env::args().nth(3).as_deref() == Some("fullhex") {
                            hex(&raw)
                        } else {
                            hex(&raw[..raw.len().min(24)])
                        }
                    );
                } else {
                    let first_int = if data.len() >= 4 {
                        i32::from_le_bytes([data[0], data[1], data[2], data[3]])
                    } else {
                        -1
                    };
                    println!(
                        "{header} first_int:{first_int} hex:{}",
                        hex(&data[..data.len().min(48)])
                    );
                }
            },
            key,
            DbKey::empty(),
        )
        .ensure();
    binlog.close(false).ensure();
}
