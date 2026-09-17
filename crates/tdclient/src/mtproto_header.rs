//! Port of `td/telegram/net/MtprotoHeader.{h,cpp}` (the net step): the
//! `invokeWithLayer{initConnection{...}}` header every session query
//! carries. The recovery-session variant (`gen_recovery_header`,
//! M.3.7.4) is the same generator pinned to `help.getConfig`; this is
//! the arbitrary-function form `Session::send` uses, driven by the
//! `Td`'s `HeaderOptions` (MtprotoHeader::Options, gathered in
//! `Td::init`).
//!
//! Byte layout note: `query` is the last field of both
//! `invokeWithLayer` and `initConnection`, so the placeholder trick
//! below serializes the whole header through the generated store and
//! swaps the final constructor word for the real function's boxed
//! bytes — the wire bytes are the C++ ones.

use crate::config_recoverer::{MTPROTO_LAYER, json_value_to_tl, recovery_language_fields};
use crate::td::HeaderOptions;
use tdmtproto::telegram_api;
use tdutils::json::json_decode;

/// `MtprotoHeader::get_default_header(options)` (MtprotoHeader.h:98):
/// the bare `invokeWithLayer{initConnection{...params}}` template — the
/// session machine appends the function's boxed bytes after it (the
/// `query` field is last; Session.cpp:279 stores this into the
/// `AuthData`).
pub(crate) fn gen_default_header(options: &HeaderOptions) -> Vec<u8> {
    let mut header = gen_mtproto_header(
        options,
        &telegram_api::Function::help_getConfig(telegram_api::help_getConfig),
    );
    // Drop the placeholder function's constructor word.
    header.truncate(header.len() - 4);
    header
}

/// `MtprotoHeader::gen_header(options, /*is_anonymous=*/ false)` with
/// `function` as the inner query (MtprotoHeader.cpp:23-110). Infallible:
/// unparsable `parameters` JSON falls back to the empty object with the
/// C++ error log (the `json_decode` failure path).
pub(crate) fn gen_mtproto_header(
    options: &HeaderOptions,
    function: &telegram_api::Function,
) -> Vec<u8> {
    let have_proxy = false; // the Mtproto-proxy option wiring is S49's runtime surface
    let mut flags = 0;
    if have_proxy {
        flags |= 1 << 0;
    }
    // `!is_anonymous` — the parameters box (MtprotoHeader.cpp:43-45).
    flags |= 1 << 1;
    if options.is_emulator {
        flags |= 1 << 10;
    }

    let mut json_value = if options.parameters.is_empty() {
        telegram_api::JSONValue::jsonObject(telegram_api::jsonObject { value: Vec::new() })
    } else {
        match json_decode(&options.parameters) {
            tdutils::status::Result::Ok(value) => json_value_to_tl(&value),
            tdutils::status::Result::Err(_) => {
                tdutils::log_error!("Failed to decode connection parameters");
                telegram_api::JSONValue::jsonObject(telegram_api::jsonObject { value: Vec::new() })
            }
        }
    };
    if let telegram_api::JSONValue::jsonObject(object) = &mut json_value {
        let tz_number = || {
            telegram_api::JSONValue::jsonNumber(telegram_api::jsonNumber {
                value: options.tz_offset as f64,
            })
        };
        match object
            .value
            .iter_mut()
            .find(|member| member.key == "tz_offset")
        {
            Some(member) => *member.value = tz_number(),
            None => object.value.push(Box::new(telegram_api::JSONObjectValue {
                key: "tz_offset".to_string(),
                value: Box::new(tz_number()),
            })),
        }
    }

    // The language rule (MtprotoHeader.cpp:56-67).
    let (lang_pack, lang_code) =
        recovery_language_fields(&options.language_pack, &options.language_code);

    let mut out = Vec::new();
    telegram_api::invokeWithLayer {
        layer: MTPROTO_LAYER,
        query: Box::new(telegram_api::Function::initConnection(
            telegram_api::initConnection {
                flags,
                api_id: options.api_id,
                device_model: options.device_model.clone(),
                system_version: options.system_version.clone(),
                app_version: options.application_version.clone(),
                system_lang_code: options.system_language_code.clone(),
                lang_pack,
                lang_code,
                proxy: telegram_api::InputClientProxy {
                    address: String::new(),
                    port: 0,
                },
                params: Box::new(json_value),
                // A unit function keeps the placeholder exactly one
                // constructor word long.
                query: Box::new(telegram_api::Function::help_getConfig(
                    telegram_api::help_getConfig,
                )),
            },
        )),
    }
    .store_boxed(&mut out);
    out.truncate(out.len() - 4);
    // `Function::store` writes the boxed (ctor + body) form.
    function.store(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header wraps the requested function: stripping the
    /// `invokeWithLayer`/`initConnection` prefix leaves the function's
    /// boxed bytes, and the prefix pins the layer 229 + the initConnection
    /// constructor with the `!is_anonymous` params box.
    #[test]
    fn header_wraps_the_function_bytes() {
        let options = HeaderOptions {
            api_id: 94575,
            system_language_code: "en-US".to_string(),
            device_model: "test-model".to_string(),
            system_version: "test-system".to_string(),
            application_version: "1.8.67".to_string(),
            language_pack: String::new(),
            language_code: String::new(),
            parameters: String::new(),
            tz_offset: 3600,
            is_emulator: false,
        };
        let function =
            telegram_api::Function::help_getAppConfig(telegram_api::help_getAppConfig { hash: -7 });
        let header = gen_mtproto_header(&options, &function);

        let mut function_bytes = Vec::new();
        function.store(&mut function_bytes);
        assert!(
            header.ends_with(&function_bytes),
            "the function is the tail"
        );

        let mut reader = telegram_api::tl::Reader::new(&header);
        assert_eq!(
            reader.i32().unwrap(),
            telegram_api::invokeWithLayer::ID,
            "invokeWithLayer"
        );
        assert_eq!(reader.i32().unwrap(), MTPROTO_LAYER, "the layer");
        assert_eq!(
            reader.i32().unwrap(),
            telegram_api::initConnection::ID,
            "initConnection"
        );
        let flags = reader.i32().unwrap();
        assert_eq!(flags & (1 << 1), 1 << 1, "the params box");
        assert_eq!(flags & (1 << 0), 0, "no proxy");
        let api_id = reader.i32().unwrap();
        assert_eq!(api_id, 94575);
    }
}
