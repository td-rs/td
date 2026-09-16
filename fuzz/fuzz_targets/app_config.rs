//! Fuzz target: the app-config application (`ConfigManager::process_app_config`,
//! ConfigManager.cpp:1321-2157) — arbitrary JSON objects through every key map
//! (bool/integer options, dice emojis, emoji sounds, premium/business features,
//! fragment prefixes, transcribe trials, freeze dates, starref prefixes) plus
//! the `convert_json_value_object` answer conversion over the filtered result.
//!
//! Input layout: one leading flag byte (bit 0 `is_authorized`, bit 1 `is_bot`,
//! bit 2 `close_flag`), the rest is the JSON text. Panic = bug: any JSON value
//! must apply as options or be ignored, never crash (the C++ reads are all
//! type-checked `get_*` helpers).

#![no_main]

use libfuzzer_sys::fuzz_target;

/// Silence the tdlib log: the C++-parity `LOG(ERROR)` rejects ("Receive
/// wrong app config", "Receive unexpected emoji sound", …) print per exec
/// at fuzz rates and would drown the campaign log. Level 0 keeps only
/// FATAL (the gate is `log_level <= verbosity_level`, so NEVER=1024
/// would log *everything*).
static QUIET_LOG: std::sync::Once = std::sync::Once::new();

fuzz_target!(|data: &[u8]| {
    QUIET_LOG.call_once(|| {
        tdutils::logging::set_verbosity_level(tdutils::logging::FATAL);
    });
    let Some((&flags, text)) = data.split_first() else {
        return;
    };
    let is_authorized = flags & 0x01 != 0;
    let is_bot = flags & 0x02 != 0;
    let close_flag = flags & 0x04 != 0;
    let Ok(text) = std::str::from_utf8(text) else {
        return;
    };
    // `json_decode` returns tdutils' own status::Result enum, not std's.
    let json = match tdutils::json::json_decode(text) {
        tdutils::status::Result::Ok(json) => json,
        tdutils::status::Result::Err(_) => return,
    };

    let config = Box::new(tdclient::config_recoverer::json_value_to_tl(&json));
    let mut manager = tdclient::config_manager::ConfigManager::new(b"");
    let mut options = tdclient::option_manager::OptionManager::new(false);
    let (filtered_config, _actions) =
        manager.process_app_config(config, &mut options, is_authorized, is_bot, close_flag);
    let _ = tdclient::config_manager::convert_json_value_object(&filtered_config);
});
