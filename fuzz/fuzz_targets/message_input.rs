//! Fuzz target: the M.10.21 td_api input plane — `ReplyMarkup::from_td_api`
//! over arbitrary markup trees built by JSON deserialization, with the five
//! gate bools (is_bot / only_inline_keyboard / request_buttons_allowed /
//! switch_inline_buttons_allowed / allow_personal) taken from the leading
//! flag byte (bit 5 selects the null-markup arm). The keyboard/inline
//! button converters (`KeyboardButton::get_keyboard_button`,
//! `get_inline_keyboard_button`) run inside. Panic = bug: the C++ answers
//! 400s for every bad input, never crashes.
//!
//! Seeds: `fuzz/seeds/message_input/` — the flag byte prepended to JSON
//! trees covering every markup kind and button type.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((flags, json)) = data.split_first_chunk::<1>() else {
        return;
    };
    let flags = flags[0];
    let is_bot = flags & 0x01 != 0;
    let only_inline_keyboard = flags & 0x02 != 0;
    let request_buttons_allowed = flags & 0x04 != 0;
    let switch_inline_buttons_allowed = flags & 0x08 != 0;
    let allow_personal = flags & 0x10 != 0;

    let markup = if flags & 0x20 != 0 {
        // the null-markup arm (sendMessage without a reply markup)
        None
    } else {
        match std::str::from_utf8(json) {
            Ok(json) => tdclient::td_api::ReplyMarkup::from_json(json).ok(),
            Err(_) => return,
        }
    };

    let _ = tdclient::messages_manager::ReplyMarkup::from_td_api(
        markup,
        is_bot,
        only_inline_keyboard,
        request_buttons_allowed,
        switch_inline_buttons_allowed,
        allow_personal,
    );
});
