//! S10 verification: builds the same telegram_api samples as the C++
//! oracle (`conformance/tl_store_oracle/dump_store.cpp`) from the
//! tl-codegen-generated model and prints one lowercase-hex line per sample.
//! The verify script diffs this against the C++ output byte for byte.
//!
//! Conditional fields are plain values (not Options), exactly like the C++
//! classes: the store gates each one on its `flags` bit, so unset bits mean
//! the placeholder value is never written.

use tdmtproto::telegram_api;

fn dump(name: &str, bytes: &[u8]) {
    print!("{name}:");
    for b in bytes {
        print!("{b:02x}");
    }
    println!();
}

/// A zeroed placeholder — never stored unless the sample's flags bits ask
/// for it, mirroring the C++ oracle's `nullptr` arguments.
fn empty_input_file() -> telegram_api::InputFile {
    telegram_api::InputFile::inputFile(telegram_api::inputFile {
        id: 0,
        parts: 0,
        name: String::new(),
        md5_checksum: String::new(),
    })
}

fn empty_video_size() -> telegram_api::VideoSize {
    telegram_api::VideoSize::videoSize(telegram_api::videoSize {
        flags: 0,
        r#type: String::new(),
        w: 0,
        h: 0,
        size: 0,
        video_start_ts: 0.0,
    })
}

fn main() {
    {
        let obj = telegram_api::inputPeerEmpty;
        let mut out = Vec::new();
        obj.store_boxed(&mut out);
        dump("inputPeerEmpty", &out);
    }
    {
        let obj = telegram_api::inputGeoPoint {
            flags: 0,
            lat: 1.5,
            long: -2.25,
            accuracy_radius: 0,
        };
        let mut out = Vec::new();
        obj.store_boxed(&mut out);
        dump("inputGeoPoint", &out);
    }
    {
        let obj = telegram_api::inputPeerUser {
            user_id: 1234567890123,
            access_hash: -9876543210987,
        };
        let mut out = Vec::new();
        obj.store_boxed(&mut out);
        dump("inputPeerUser", &out);
    }
    {
        let obj = telegram_api::inputMediaContact {
            phone_number: "+1-555-0100".to_owned(),
            first_name: "x".repeat(300),
            last_name: "Smith".to_owned(),
            vcard: "VCARD".to_owned(),
        };
        let mut out = Vec::new();
        obj.store_boxed(&mut out);
        dump("inputMediaContact", &out);
    }
    {
        let obj = telegram_api::messages_getChats { id: vec![1, 2, 3] };
        let mut out = Vec::new();
        obj.store_boxed(&mut out);
        dump("messages_getChats", &out);
    }
    {
        let obj = telegram_api::messages_getDialogs {
            flags: 0,
            exclude_pinned: true,
            folder_id: 42,
            offset_date: 0,
            offset_id: 0,
            offset_peer: Some(Box::new(telegram_api::InputPeer::inputPeerEmpty(
                telegram_api::inputPeerEmpty,
            ))),
            limit: 0,
            hash: 0,
        };
        let mut out = Vec::new();
        obj.store_boxed(&mut out);
        dump("messages_getDialogs_flags0", &out);
    }
    {
        let obj = telegram_api::messages_getDialogs {
            flags: 2,
            exclude_pinned: false,
            folder_id: 42,
            offset_date: 0,
            offset_id: 0,
            offset_peer: Some(Box::new(telegram_api::InputPeer::inputPeerEmpty(
                telegram_api::inputPeerEmpty,
            ))),
            limit: 0,
            hash: 0,
        };
        let mut out = Vec::new();
        obj.store_boxed(&mut out);
        dump("messages_getDialogs_flags2", &out);
    }
    {
        let obj = telegram_api::messages_deleteMessages {
            flags: 0,
            revoke: true,
            id: vec![1, 2],
        };
        let mut out = Vec::new();
        obj.store_boxed(&mut out);
        dump("messages_deleteMessages", &out);
    }
    {
        let obj = telegram_api::inputChatUploadedPhoto {
            flags: 0,
            file: Some(empty_input_file()),
            video: Some(empty_input_file()),
            video_start_ts: 0.0,
            video_emoji_markup: Some(empty_video_size()),
        };
        let mut out = Vec::new();
        obj.store_boxed(&mut out);
        dump("inputChatUploadedPhoto_flags0", &out);
    }
    {
        let obj = telegram_api::inputChatUploadedPhoto {
            flags: 15,
            file: Some(telegram_api::InputFile::inputFile(
                telegram_api::inputFile {
                    id: 1,
                    parts: 2,
                    name: "name".to_owned(),
                    md5_checksum: "md5".to_owned(),
                },
            )),
            video: Some(telegram_api::InputFile::inputFile(
                telegram_api::inputFile {
                    id: 3,
                    parts: 4,
                    name: "video".to_owned(),
                    md5_checksum: "sum".to_owned(),
                },
            )),
            video_start_ts: 1.5,
            video_emoji_markup: Some(telegram_api::VideoSize::videoSize(
                telegram_api::videoSize {
                    flags: 0,
                    r#type: "u".to_owned(),
                    w: 640,
                    h: 360,
                    size: 0,
                    video_start_ts: 0.0,
                },
            )),
        };
        let mut out = Vec::new();
        obj.store_boxed(&mut out);
        dump("inputChatUploadedPhoto_flags15", &out);
    }
}
