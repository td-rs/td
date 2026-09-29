//! `Payments` (td/telegram/Payments.{h,cpp}) — the M.43 Wave-3 slice: the
//! whole payment value plane (the input-invoice conversions, the payment
//! form/result/receipt builders, the order-info plane) and the payment
//! request flows. Payments.cpp 1.8.67 is free functions over `Td` (there
//! is no PaymentsManager actor); the port keeps that shape — every C++
//! `void f(Td *td, ..., Promise<T> &&)` becomes
//! `pub fn f(env: &mut impl PaymentsEnv, ..., Promise<T>)`.
//!
//! Pulled-in helper planes the header includes (each kept at its C++
//! anchor):
//! - `LabeledPricePart` (LabeledPricePart.{h,cpp}) — the price-part value
//!   type with its validation and wire conversion.
//! - `check_currency_amount` (misc.cpp:390-393) — lives in [`crate::misc`].
//! - `check_payment_amount` (Premium.cpp:184-191).
//! - `StarManager::get_star_count` (StarManager.cpp:1799-1817) — the
//!   single-argument form the payment plane calls.
//! - `PasswordManager::get_temp_password_state_sync`
//!   (PasswordManager.cpp:276-284) over the TempPasswordState.hpp store
//!   format — realized through the env's binlog-pmc seam.
//! - `GiveawayParameters` (GiveawayParameters.{h,cpp}) — the giveaway
//!   value struct with its validation and the two wire-purpose builders.
//! - `BusinessConnectionId` (BusinessConnectionId.h) — the string-id
//!   wrapper (`is_valid`/`get`; the `get_invoke_prefix` wire half is the
//!   net step's).
//! - `InputInvoice`'s value half (InputInvoice.{h,cpp}): the inner
//!   `Invoice` struct with `get_input_invoice`
//!   (InputInvoice.cpp:225-244), `process_input_message_invoice`
//!   (InputInvoice.cpp:80-276) and `get_input_media_invoice`
//!   (InputInvoice.cpp:294-324), plus `get_product_info_object`
//!   (InputInvoice.cpp:400-408).
//! - `ThemeManager::get_theme_parameters_json_string`
//!   (ThemeManager.cpp:609-637) — the pure static the getPaymentForm
//!   themeParameters hop needs (the M.41 slice owns the canonical copy in
//!   its base; this base is pre-M.41).
//!
//! Seams ([`PaymentsEnv`], the ThemeManagerEnv convention):
//! - The eleven `Td::ResultHandler` queries are `[blocked: net]` env
//!   seams: each `send_*` seam `log_info!`s the query name and parks (by
//!   consuming) the request promise — the deferred outbox keeps the
//!   request pending exactly like a sent-but-unanswered net query. The
//!   matching `on_get_*` answer halves are real and unit-tested; they are
//!   what the net integration will drive (the M.31/M.41 convention).
//! - The MessagesManager reads (`get_invoice_message_info`,
//!   `get_payment_successful_message_id`, MessagesManager.cpp:36587-36675)
//!   are `[blocked: M.10]` env seams.
//! - The StarManager balance plane (`has_owned_star_count`,
//!   `add_pending_owned_star_count`), the suggested-action dismiss, the
//!   BusinessConnectionManager check, `get_premium_gift_text`
//!   (Premium.cpp:209-222 — the full port rides `get_formatted_text` +
//!   `get_input_text_with_entities` and lands with the Premium slice) and
//!   `LinkManager::get_dialog_invite_link_hash` (LinkManager.cpp:4259 —
//!   the URL stack is Wave 2) are env seams with optimistic defaults.
//! - The Files plane (`get_web_document_photo`, Photo.cpp:117-131;
//!   `get_photo_object`, Photo.h:79; `from_persistent_id`;
//!   `get_input_web_document`, InputInvoice.cpp:246-282) is
//!   `[blocked: M.12]` seams; the payment forms/receipts build photo-less
//!   product info until it lands.
//! - `DialogManager::on_get_dialog_error` (the ResultHandler `on_error`
//!   intake) rides an env seam — the real free function needs the whole
//!   `Td`, so the hookup lands with the net step.
//!
//! Recorded divergences:
//! - `convert_labeled_price`/`convert_invoice` mutate the received wire
//!   object in place on invalid input (the C++ moves it through); the
//!   Rust takes the fields by value and rebuilds the clamped forms.
//! - The product-info description entities are built manager-less (the
//!   C++ passes `td->user_manager_` only for the mentionName side
//!   effects; the port's `UserManagerLike` keeps the raw id, so the
//!   object output is identical).
//! - `get_input_web_document` (the invoice photo → InputWebDocument
//!   collapse) needs the FileManager file-view URL; the env seam returns
//!   the raw URL and the wire form is built here (InputInvoice.cpp:246-282
//!   is preserved modulo the file-view and MimeType hops, whose constant
//!   image/jpeg form matches the invoice-photo call shape).

use crate::dialog_id::{DialogId, MessageFullId, ServerMessageId};
use crate::message_id::MessageId;
use crate::photo::Photo;
use crate::store_parse::Parser;
use crate::td_api;
use crate::user_manager::{AccessRights, UserId};
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::{Result, Status};

// --- the wire flag masks (the C++ `telegram_api::*::*_MASK` constants,
// telegram_api.tl flag positions) ---
const INVOICE_MAX_TIP_AMOUNT_MASK: i32 = 1 << 8;
const INVOICE_SUBSCRIPTION_PERIOD_MASK: i32 = 1 << 11;
const INVOICE_TERMS_URL_MASK: i32 = 1 << 10;
const PAYMENT_REQUESTED_INFO_NAME_MASK: i32 = 1;
const PAYMENT_REQUESTED_INFO_PHONE_MASK: i32 = 1 << 1;
const PAYMENT_REQUESTED_INFO_EMAIL_MASK: i32 = 1 << 2;
const PAYMENT_REQUESTED_INFO_SHIPPING_ADDRESS_MASK: i32 = 1 << 3;
const INPUT_INVOICE_PREMIUM_GIFT_STARS_MESSAGE_MASK: i32 = 1;
const INPUT_STORE_PREMIUM_GIFT_CODE_BOOST_PEER_MASK: i32 = 1;
const INPUT_STORE_PREMIUM_GIFT_CODE_MESSAGE_MASK: i32 = 1 << 1;
const INPUT_STORE_STARS_TOPUP_SPEND_PURPOSE_PEER_MASK: i32 = 1;
const INPUT_STORE_PREMIUM_GIVEAWAY_ADDITIONAL_PEERS_MASK: i32 = 1 << 1;
const INPUT_STORE_PREMIUM_GIVEAWAY_COUNTRIES_ISO2_MASK: i32 = 1 << 2;
const INPUT_STORE_PREMIUM_GIVEAWAY_PRIZE_DESCRIPTION_MASK: i32 = 1 << 4;
const INPUT_MEDIA_INVOICE_PHOTO_MASK: i32 = 1;
const INPUT_MEDIA_INVOICE_START_PARAM_MASK: i32 = 1 << 1;
const INPUT_MEDIA_INVOICE_EXTENDED_MEDIA_MASK: i32 = 1 << 2;
const INPUT_MEDIA_INVOICE_PROVIDER_MASK: i32 = 1 << 3;

// ---------------------------------------------------------------------------
// the value plane
// ---------------------------------------------------------------------------

/// `td::LabeledPricePart` (LabeledPricePart.h:18-51).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LabeledPricePart {
    pub label: String,
    pub amount: i64,
}

impl LabeledPricePart {
    /// `LabeledPricePart::get_labeled_price_parts` (LabeledPricePart.cpp:24-55).
    /// `result_total_amount` is `None` for the C++ `nullptr`.
    pub fn get_labeled_price_parts(
        parts: Vec<td_api::LabeledPricePart>,
        result_total_amount: Option<&mut i64>,
    ) -> Result<Vec<LabeledPricePart>> {
        if parts.is_empty() {
            return Result::from_status(Status::error(
                400,
                "There must be at least one price part",
            ));
        }
        let mut result = Vec::with_capacity(parts.len());
        let mut total_amount: i64 = 0;
        for part in parts {
            let Some(label) = crate::misc::clean_input_string(&part.label) else {
                return Result::from_status(Status::error(
                    400,
                    "Price part label must be encoded in UTF-8",
                ));
            };
            if !crate::misc::check_currency_amount(part.amount) {
                return Result::from_status(Status::error(
                    400,
                    "Too big amount of the currency specified",
                ));
            }
            total_amount += part.amount;
            result.push(LabeledPricePart {
                label,
                amount: part.amount,
            });
        }
        if total_amount <= 0 {
            return Result::from_status(Status::error(400, "Total price must be positive"));
        }
        if !crate::misc::check_currency_amount(total_amount) {
            return Result::from_status(Status::error(400, "Total price is too big"));
        }
        if let Some(total) = result_total_amount {
            *total = total_amount;
        }
        Result::Ok(result)
    }

    /// `LabeledPricePart::get_input_labeled_price` (LabeledPricePart.cpp:15-17).
    fn get_input_labeled_price(&self) -> telegram_api::LabeledPrice {
        telegram_api::LabeledPrice {
            label: self.label.clone(),
            amount: self.amount,
        }
    }

    /// `LabeledPricePart::get_input_labeled_prices` (LabeledPricePart.cpp:19-22).
    pub fn get_input_labeled_prices(parts: &[LabeledPricePart]) -> Vec<telegram_api::LabeledPrice> {
        parts.iter().map(Self::get_input_labeled_price).collect()
    }
}

/// `convert_labeled_price` (Payments.cpp:293-301): the invalid amounts are
/// clamped to the ±2^40 sentinel with the LOG(ERROR).
fn convert_labeled_price(labeled_price: telegram_api::LabeledPrice) -> td_api::LabeledPricePart {
    let mut amount = labeled_price.amount;
    if !crate::misc::check_currency_amount(amount) {
        tdutils::log_error!("Receive invalid labeled price amount {amount}");
        amount = if amount < 0 { -1 } else { 1 } * (1i64 << 40);
    }
    td_api::LabeledPricePart {
        label: labeled_price.label,
        amount,
    }
}

/// `convert_invoice` (Payments.cpp:303-345).
fn convert_invoice(invoice: telegram_api::Invoice) -> td_api::Invoice {
    let mut invoice = invoice;
    let labeled_prices: Vec<td_api::LabeledPricePart> = invoice
        .prices
        .into_iter()
        .map(convert_labeled_price)
        .collect();
    let mut need_phone_number = invoice.phone_requested;
    let mut need_email_address = invoice.email_requested;
    let mut need_shipping_address = invoice.shipping_address_requested;
    let send_phone_number_to_provider = invoice.phone_to_provider;
    let send_email_address_to_provider = invoice.email_to_provider;
    let is_flexible = invoice.flexible;
    if send_phone_number_to_provider {
        need_phone_number = true;
    }
    if send_email_address_to_provider {
        need_email_address = true;
    }
    if is_flexible {
        need_shipping_address = true;
    }

    if invoice.max_tip_amount < 0 || !crate::misc::check_currency_amount(invoice.max_tip_amount) {
        tdutils::log_error!(
            "Receive invalid maximum tip amount {}",
            invoice.max_tip_amount
        );
        invoice.max_tip_amount = 0;
    }
    invoice
        .suggested_tip_amounts
        .retain(|&amount| amount >= 0 && crate::misc::check_currency_amount(amount));
    if invoice.suggested_tip_amounts.len() > 4 {
        invoice.suggested_tip_amounts.resize(4, 0);
    }

    let (recurring_terms_url, terms_url) = if invoice.recurring {
        (invoice.terms_url, String::new())
    } else {
        (String::new(), invoice.terms_url)
    };
    td_api::Invoice {
        currency: invoice.currency,
        price_parts: labeled_prices,
        subscription_period: invoice.subscription_period.max(0),
        max_tip_amount: invoice.max_tip_amount,
        suggested_tip_amounts: invoice.suggested_tip_amounts,
        recurring_payment_terms_of_service_url: recurring_terms_url,
        terms_of_service_url: terms_url,
        is_test: invoice.test,
        need_name: invoice.name_requested,
        need_phone_number,
        need_email_address,
        need_shipping_address,
        send_phone_number_to_provider,
        send_email_address_to_provider,
        is_flexible,
    }
}

/// `convert_payment_provider` (Payments.cpp:347-421): the smartglocal /
/// stripe native-provider JSON collapse; `None` is the C++ null return.
fn convert_payment_provider(
    native_provider_name: &str,
    native_parameters: Option<telegram_api::DataJSON>,
    is_test: bool,
) -> Option<td_api::PaymentProvider> {
    use tdutils::json::{JsonValue, json_decode};

    let native_parameters = native_parameters?;
    if native_provider_name == "smartglocal" {
        let data = native_parameters.data;
        let value = match json_decode(&data) {
            Result::Ok(value) => value,
            Result::Err(error) => {
                tdutils::log_error!("Can't parse JSON object \"{data}\": {error}");
                return None;
            }
        };
        let JsonValue::Object(object) = value else {
            tdutils::log_error!("Wrong JSON data \"{data}\"");
            return None;
        };
        let public_token = match object.get_required_string_field("public_token") {
            Result::Ok(public_token) => public_token,
            Result::Err(_) => {
                tdutils::log_error!("Unsupported JSON data \"{data}\"");
                return None;
            }
        };
        let mut tokenize_url = if is_test {
            "https://tgb-playground.smart-glocal.com/cds/v1/tokenize/card"
        } else {
            "https://tgb.smart-glocal.com/cds/v1/tokenize/card"
        }
        .to_owned();
        if let Result::Ok(tokenize_url_override) =
            object.get_optional_string_field("tokenize_url", "")
            && tokenize_url_override.starts_with("https://")
            && tokenize_url_override.ends_with(".smart-glocal.com/cds/v1/tokenize/card")
        {
            tokenize_url = tokenize_url_override;
        }
        if object.field_count() > 2 {
            tdutils::log_error!("Unsupported JSON data \"{data}\"");
        }
        return Some(td_api::PaymentProvider::paymentProviderSmartGlocal(
            td_api::paymentProviderSmartGlocal {
                public_token,
                tokenize_url,
            },
        ));
    }
    if native_provider_name == "stripe" {
        let data = native_parameters.data;
        let value = match json_decode(&data) {
            Result::Ok(value) => value,
            Result::Err(error) => {
                tdutils::log_error!("Can't parse JSON object \"{data}\": {error}");
                return None;
            }
        };
        let JsonValue::Object(object) = value else {
            tdutils::log_error!("Wrong JSON data \"{data}\"");
            return None;
        };
        let (need_country, need_postal_code, need_cardholder_name, publishable_key) = match (
            object.get_required_bool_field("need_country"),
            object.get_required_bool_field("need_zip"),
            object.get_required_bool_field("need_cardholder_name"),
            object.get_required_string_field("publishable_key"),
        ) {
            (
                Result::Ok(need_country),
                Result::Ok(need_postal_code),
                Result::Ok(need_cardholder_name),
                Result::Ok(publishable_key),
            ) => (
                need_country,
                need_postal_code,
                need_cardholder_name,
                publishable_key,
            ),
            _ => {
                tdutils::log_error!("Unsupported JSON data \"{data}\"");
                return None;
            }
        };
        // TODO support "gpay_parameters":{"gateway":"stripe",
        // "stripe:publishableKey":"...","stripe:version":"..."}
        if object.field_count() != 5 {
            tdutils::log_error!("Unsupported JSON data \"{data}\"");
        }
        return Some(td_api::PaymentProvider::paymentProviderStripe(
            td_api::paymentProviderStripe {
                publishable_key,
                need_country,
                need_postal_code,
                need_cardholder_name,
            },
        ));
    }

    None
}

/// `convert_address(postAddress)` (Payments.cpp:423-430).
fn convert_address_to_td(address: Option<telegram_api::PostAddress>) -> Option<td_api::Address> {
    let address = address?;
    Some(td_api::Address {
        country_code: address.country_iso2,
        state: address.state,
        city: address.city,
        street_line1: address.street_line1,
        street_line2: address.street_line2,
        postal_code: address.post_code,
    })
}

/// `convert_address(td_api::address)` (Payments.cpp:432-439) — the C++
/// overload set becomes a suffixed pair.
fn convert_address_to_telegram(
    address: Option<td_api::Address>,
) -> Option<telegram_api::PostAddress> {
    let address = address?;
    Some(telegram_api::PostAddress {
        street_line1: address.street_line1,
        street_line2: address.street_line2,
        city: address.city,
        state: address.state,
        country_iso2: address.country_code,
        post_code: address.postal_code,
    })
}

/// `convert_order_info(paymentRequestedInfo)` (Payments.cpp:441-449).
fn convert_order_info_to_td(
    order_info: Option<telegram_api::PaymentRequestedInfo>,
) -> Option<td_api::OrderInfo> {
    let order_info = order_info?;
    Some(td_api::OrderInfo {
        name: order_info.name,
        phone_number: order_info.phone,
        email_address: order_info.email,
        shipping_address: convert_address_to_td(order_info.shipping_address),
    })
}

/// `convert_order_info(td_api::orderInfo)` (Payments.cpp:461-482).
fn convert_order_info_to_telegram(
    mut order_info: Option<td_api::OrderInfo>,
) -> Option<telegram_api::PaymentRequestedInfo> {
    let mut order_info = order_info.take()?;
    let mut flags = 0;
    if !order_info.name.is_empty() {
        flags |= PAYMENT_REQUESTED_INFO_NAME_MASK;
    }
    if !order_info.phone_number.is_empty() {
        flags |= PAYMENT_REQUESTED_INFO_PHONE_MASK;
    }
    if !order_info.email_address.is_empty() {
        flags |= PAYMENT_REQUESTED_INFO_EMAIL_MASK;
    }
    if order_info.shipping_address.is_some() {
        flags |= PAYMENT_REQUESTED_INFO_SHIPPING_ADDRESS_MASK;
    }
    Some(telegram_api::PaymentRequestedInfo {
        flags,
        name: std::mem::take(&mut order_info.name),
        phone: std::mem::take(&mut order_info.phone_number),
        email: std::mem::take(&mut order_info.email_address),
        shipping_address: convert_address_to_telegram(order_info.shipping_address),
    })
}

/// `convert_shipping_option` (Payments.cpp:451-459).
fn convert_shipping_option(
    shipping_option: Option<telegram_api::ShippingOption>,
) -> Option<td_api::ShippingOption> {
    let shipping_option = shipping_option?;
    Some(td_api::ShippingOption {
        id: shipping_option.id,
        title: shipping_option.title,
        price_parts: shipping_option
            .prices
            .into_iter()
            .map(convert_labeled_price)
            .collect(),
    })
}

/// `convert_saved_credentials` (Payments.cpp:484-490).
fn convert_saved_credentials(
    saved_credentials: Vec<telegram_api::PaymentSavedCredentials>,
) -> Vec<td_api::SavedCredentials> {
    saved_credentials
        .into_iter()
        .map(|credentials| td_api::SavedCredentials {
            id: credentials.id,
            title: credentials.title,
        })
        .collect()
}

/// `StarManager::get_star_count(amount)` (StarManager.cpp:1799-1817) —
/// the `allow_negative = false` default the payment plane calls.
fn get_star_count(amount: i64) -> i64 {
    let max_amount = 1i64 << 51;
    if amount < 0 {
        tdutils::log_error!("Receive Telegram Star amount = {amount}");
        return 0;
    }
    if amount > max_amount {
        tdutils::log_error!("Receive Telegram Star amount = {amount}");
        return max_amount;
    }
    amount
}

/// `check_payment_amount` (Premium.cpp:184-191): validates the amount and
/// UTF-8-cleans the currency in place.
fn check_payment_amount(currency: &mut String, amount: i64) -> Result<()> {
    if amount <= 0 || !crate::misc::check_currency_amount(amount) {
        return Result::from_status(Status::error(
            400,
            "Invalid amount of the currency specified",
        ));
    }
    let Some(cleaned) = crate::misc::clean_input_string(currency) else {
        return Result::from_status(Status::error(400, "Strings must be encoded in UTF-8"));
    };
    *currency = cleaned;
    Result::Ok(())
}

/// `ThemeManager::get_theme_parameters_json_string`
/// (ThemeManager.cpp:609-637) — the pure static behind the synchronous
/// getThemeParametersJsonString (SynchronousRequests.cpp:325-329); the
/// JSON object keeps the C++ insertion order and the `#rrggbb` lowercase
/// color form.
pub fn get_theme_parameters_json_string(theme_parameters: &td_api::ThemeParameters) -> String {
    let get_color = |color: i32| {
        let mut res = ['#'; 7];
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for (i, shift) in [(0usize, 16i32), (1, 8), (2, 0)] {
            let num = ((color >> shift) & 0xFF) as usize;
            res[2 * i + 1] = HEX[num >> 4] as char;
            res[2 * i + 2] = HEX[num & 15] as char;
        }
        res.iter().collect::<String>()
    };
    let fields = [
        ("bg_color", theme_parameters.background_color),
        (
            "secondary_bg_color",
            theme_parameters.secondary_background_color,
        ),
        ("text_color", theme_parameters.text_color),
        ("hint_color", theme_parameters.hint_color),
        ("link_color", theme_parameters.link_color),
        ("button_color", theme_parameters.button_color),
        ("button_text_color", theme_parameters.button_text_color),
        ("header_bg_color", theme_parameters.header_background_color),
        (
            "bottom_bar_bg_color",
            theme_parameters.bottom_bar_background_color,
        ),
        (
            "section_bg_color",
            theme_parameters.section_background_color,
        ),
        (
            "section_separator_color",
            theme_parameters.section_separator_color,
        ),
        ("accent_text_color", theme_parameters.accent_text_color),
        (
            "section_header_text_color",
            theme_parameters.section_header_text_color,
        ),
        ("subtitle_text_color", theme_parameters.subtitle_text_color),
        (
            "destructive_text_color",
            theme_parameters.destructive_text_color,
        ),
    ];
    let body: Vec<String> = fields
        .into_iter()
        .map(|(key, value)| format!("\"{key}\":\"{}\"", get_color(value)))
        .collect();
    format!("{{{}}}", body.join(","))
}

/// `td::TempPasswordState` (TempPasswordState.h:15-19) — the slice
/// `get_temp_password_state_sync` consumes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TempPasswordState {
    pub has_temp_password: bool,
    pub temp_password: String,
    pub valid_until: i32, // unix_time
}

/// `PasswordManager::get_temp_password_state_sync`
/// (PasswordManager.cpp:276-284) over the env's binlog-pmc and clock; the
/// row format is the TempPasswordState.hpp store (string + int32 behind a
/// `has_temp_password` check). A corrupt row reads as the default state,
/// exactly like the C++ `log_event_parse` failure arm.
pub fn get_temp_password_state_sync(env: &mut impl PaymentsEnv) -> TempPasswordState {
    let temp_password_str = env.binlog_pmc_get_raw(b"temp_password");
    let mut res = TempPasswordState::default();
    if !temp_password_str.is_empty() {
        // `log_event_parse` framing (PasswordManager.cpp:333) — the
        // log-event version prefix leads the row.
        let payload = temp_password_str
            .strip_prefix(
                crate::store_parse::version::LOG_EVENT_STORE_VERSION
                    .to_le_bytes()
                    .as_slice(),
            )
            .unwrap_or(&temp_password_str);
        let parse = || -> std::result::Result<TempPasswordState, &'static str> {
            let mut parser = Parser::new(payload);
            let temp_password = parser.string()?.to_owned();
            let valid_until = parser.i32_()?;
            std::result::Result::Ok(TempPasswordState {
                has_temp_password: true,
                temp_password,
                valid_until,
            })
        };
        if let std::result::Result::Ok(parsed) = parse() {
            res = parsed;
        }
    }
    if !res.has_temp_password || res.valid_until <= env.get_unix_time() {
        res = TempPasswordState::default();
    }
    res
}

/// `td::BusinessConnectionId` (BusinessConnectionId.h:17-58) — the
/// string-id wrapper; the `get_invoke_prefix` wire half is the net step's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BusinessConnectionId(String);

impl BusinessConnectionId {
    pub fn new(business_connection_id: String) -> BusinessConnectionId {
        BusinessConnectionId(business_connection_id)
    }

    /// `BusinessConnectionId::is_valid` — non-empty.
    pub fn is_valid(&self) -> bool {
        !self.0.is_empty()
    }

    pub fn get(&self) -> &str {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// the InputInvoice value half (InputInvoice.{h,cpp})
// ---------------------------------------------------------------------------

/// `td::MessageExtendedMedia`'s export-invoice slice — the paid-media
/// plane is [blocked: the PaidMedia slice]; the port carries only the
/// `is_empty()` state the `get_input_media_invoice` flow consumes
/// (MessageExtendedMedia.cpp:29-59 collapses to the presence flag).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExtendedMedia {
    present: bool,
}

impl ExtendedMedia {
    /// `MessageExtendedMedia::is_empty`.
    pub fn is_empty(&self) -> bool {
        !self.present
    }
}

/// `InputInvoice::Invoice` (InputInvoice.h:22-58) — the inner invoice
/// value struct; every field defaults to zero/false like the C++.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Invoice {
    pub currency: String,
    pub price_parts: Vec<LabeledPricePart>,
    pub subscription_period: i32,
    pub max_tip_amount: i64,
    pub suggested_tip_amounts: Vec<i64>,
    pub recurring_payment_terms_of_service_url: String,
    pub terms_of_service_url: String,
    pub is_test: bool,
    pub need_name: bool,
    pub need_phone_number: bool,
    pub need_email_address: bool,
    pub need_shipping_address: bool,
    pub send_phone_number_to_provider: bool,
    pub send_email_address_to_provider: bool,
    pub is_flexible: bool,
}

impl Invoice {
    /// `InputInvoice::Invoice::get_input_invoice` (InputInvoice.cpp:225-244).
    pub fn get_input_invoice(&self) -> telegram_api::Invoice {
        let mut flags = 0;
        if self.max_tip_amount != 0 {
            flags |= INVOICE_MAX_TIP_AMOUNT_MASK;
        }
        if self.subscription_period != 0 {
            flags |= INVOICE_SUBSCRIPTION_PERIOD_MASK;
        }
        let mut is_recurring = false;
        let mut terms_of_service_url = String::new();
        if !self.recurring_payment_terms_of_service_url.is_empty() {
            is_recurring = true;
            flags |= INVOICE_TERMS_URL_MASK;
            terms_of_service_url = self.recurring_payment_terms_of_service_url.clone();
        } else if !self.terms_of_service_url.is_empty() {
            flags |= INVOICE_TERMS_URL_MASK;
            terms_of_service_url = self.terms_of_service_url.clone();
        }

        telegram_api::Invoice {
            flags,
            test: self.is_test,
            name_requested: self.need_name,
            phone_requested: self.need_phone_number,
            email_requested: self.need_email_address,
            shipping_address_requested: self.need_shipping_address,
            flexible: self.is_flexible,
            phone_to_provider: self.send_phone_number_to_provider,
            email_to_provider: self.send_email_address_to_provider,
            recurring: is_recurring,
            currency: self.currency.clone(),
            prices: LabeledPricePart::get_input_labeled_prices(&self.price_parts),
            max_tip_amount: self.max_tip_amount,
            suggested_tip_amounts: self.suggested_tip_amounts.clone(),
            terms_url: terms_of_service_url,
            subscription_period: self.subscription_period,
        }
    }
}

/// `td::InputInvoice` (InputInvoice.h) — the export-invoice slice. The
/// message/inline-message constructors, the store/parse and the
/// message-object halves are their own plane; the port carries the fields
/// `process_input_message_invoice` fills.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputInvoice {
    pub title: String,
    pub description: String,
    pub photo: Photo,
    pub start_parameter: String,
    pub invoice: Invoice,
    pub payload: Vec<u8>,
    pub provider_token: String,
    pub provider_data: String,
    pub extended_media: ExtendedMedia,
    pub extended_media_caption: crate::message_entity::FormattedText,
    pub total_amount: i64,
    pub receipt_message_id: MessageId,
}

/// `get_input_web_document` (InputInvoice.cpp:246-282) — the photo →
/// InputWebDocument collapse; the FileManager file-view URL hop is the
/// [`PaymentsEnv::file_url`] seam.
fn get_input_web_document(
    env: &mut impl PaymentsEnv,
    photo: &Photo,
) -> Option<telegram_api::InputWebDocument> {
    if photo.is_empty() {
        return None;
    }

    assert!(photo.photos.len() == 1);
    let size = &photo.photos[0];
    assert!(size.file_id.is_valid());

    let mut attributes: Vec<telegram_api::DocumentAttribute> = Vec::new();
    if size.dimensions.width != 0 && size.dimensions.height != 0 {
        attributes.push(telegram_api::DocumentAttribute::documentAttributeImageSize(
            telegram_api::documentAttributeImageSize {
                w: size.dimensions.width as i32,
                h: size.dimensions.height as i32,
            },
        ));
    }

    let url = env.file_url(size.file_id)?;
    Some(telegram_api::InputWebDocument {
        url,
        size: size.size,
        // the invoice-photo call shape is always a JPEG (the C++
        // MimeType::from_url hop collapses to it here)
        mime_type: "image/jpeg".to_owned(),
        attributes,
    })
}

impl InputInvoice {
    /// `InputInvoice::get_input_media_invoice` (InputInvoice.cpp:294-324)
    /// for the export-invoice call shape (`input_file`/`input_thumbnail`
    /// are `nullptr` there). `None` models the C++ null return.
    pub fn get_input_media_invoice(
        &self,
        env: &mut impl PaymentsEnv,
    ) -> Option<telegram_api::inputMediaInvoice> {
        let mut flags = 0;
        if !self.start_parameter.is_empty() {
            flags |= INPUT_MEDIA_INVOICE_START_PARAM_MASK;
        }
        let input_web_document = get_input_web_document(env, &self.photo);
        if input_web_document.is_some() {
            flags |= INPUT_MEDIA_INVOICE_PHOTO_MASK;
        }
        let mut extended_media: Option<telegram_api::InputMedia> = None;
        if !self.extended_media.is_empty() {
            flags |= INPUT_MEDIA_INVOICE_EXTENDED_MEDIA_MASK;
            extended_media = env.extended_media_input_media();
            extended_media.as_ref()?;
        }
        if !self.provider_token.is_empty() {
            flags |= INPUT_MEDIA_INVOICE_PROVIDER_MASK;
        }

        Some(telegram_api::inputMediaInvoice {
            flags,
            title: self.title.clone(),
            description: self.description.clone(),
            photo: input_web_document,
            invoice: Some(self.invoice.get_input_invoice()),
            payload: self.payload.clone(),
            provider: self.provider_token.clone(),
            provider_data: Some(telegram_api::DataJSON {
                data: if self.provider_data.is_empty() {
                    "null".to_owned()
                } else {
                    self.provider_data.clone()
                },
            }),
            start_param: self.start_parameter.clone(),
            extended_media: extended_media.map(Box::new),
        })
    }
}

// ---------------------------------------------------------------------------
// the env trait
// ---------------------------------------------------------------------------

/// The `Td`/`G()` surface of the payment flows. The net queries are the
/// `[blocked: net]` seams; the dialog/message reads are the
/// `[blocked: M.10]` stub-inventory seams.
pub trait PaymentsEnv {
    // --- the G()/OptionManager plane ---
    /// `G()->unix_time()`.
    fn get_unix_time(&self) -> i32;

    /// `td_->option_manager_->get_option_integer(name)` (the C++ default 0).
    fn get_option_integer(&mut self, name: &str) -> i64;

    /// `G()->td_db()->get_binlog_pmc()->get`.
    fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;

    // --- the UserManager plane (real calls on the Td side) ---
    /// `td_->user_manager_->get_input_user(user_id)`.
    fn get_input_user(&mut self, user_id: UserId) -> Result<telegram_api::InputUser>;

    /// `td_->user_manager_->get_input_users(user_ids)`.
    fn get_input_users(&mut self, user_ids: &[UserId]) -> Result<Vec<telegram_api::InputUser>>;

    /// `td_->user_manager_->on_get_users(users, source)`.
    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str);

    /// `td_->user_manager_->get_user_id_object(user_id, source)`.
    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64;

    // --- the DialogManager/ChatManager plane (the M.6 stub inventory) ---
    /// `td_->dialog_manager_->have_dialog_force(dialog_id, source)`.
    fn have_dialog_force(&mut self, dialog_id: DialogId, source: &str) -> bool;

    /// `td_->chat_manager_->get_channel_status(channel_id)`.
    fn get_channel_status(
        &mut self,
        channel_id: crate::chat_manager::ChannelId,
    ) -> crate::chat_manager::DialogParticipantStatus;

    /// `td_->chat_manager_->is_broadcast_channel(channel_id)`.
    fn is_broadcast_channel(&mut self, channel_id: crate::chat_manager::ChannelId) -> bool;

    /// `td_->dialog_manager_->get_input_peer(dialog_id, access_rights)`.
    fn get_input_peer(
        &mut self,
        dialog_id: DialogId,
        access_rights: AccessRights,
    ) -> Option<telegram_api::InputPeer>;

    /// `td_->dialog_manager_->check_dialog_access(dialog_id, false,
    /// AccessRights::Read, source)` — the payment plane's only call shape.
    fn check_dialog_access(&mut self, dialog_id: DialogId, source: &str) -> Result<()>;

    /// `td_->dialog_manager_->get_my_dialog_id()`.
    fn get_my_dialog_id(&self) -> DialogId;

    /// `td_->dialog_manager_->on_get_dialog_error(dialog_id, status,
    /// source)` — the ResultHandler `on_error` intake; the real hookup
    /// lands with the net step.
    fn on_get_dialog_error(&mut self, _dialog_id: DialogId, _status: &Status, _source: &str) {}

    // --- the MessagesManager seams ([blocked: M.10]) ---
    /// `td_->messages_manager_->get_invoice_message_info(message_full_id)`
    /// (MessagesManager.cpp:36587-36624).
    fn get_invoice_message_info(
        &mut self,
        message_full_id: MessageFullId,
    ) -> Result<InvoiceMessageInfo> {
        let _ = message_full_id;
        tdutils::log_info!(
            "MessagesManager::get_invoice_message_info — [blocked: M.10 MessagesManager]"
        );
        Result::from_status(Status::error(400, "Message not found"))
    }

    /// `td_->messages_manager_->get_payment_successful_message_id(...)`
    /// (MessagesManager.cpp:36658-36675).
    fn get_payment_successful_message_id(
        &mut self,
        message_full_id: MessageFullId,
    ) -> Result<ServerMessageId> {
        let _ = message_full_id;
        tdutils::log_info!(
            "MessagesManager::get_payment_successful_message_id — [blocked: M.10 MessagesManager]"
        );
        Result::from_status(Status::error(400, "Message not found"))
    }

    // --- the StarManager / suggested-action seams ---
    /// `td_->star_manager_->has_owned_star_count(star_count)`.
    fn has_owned_star_count(&mut self, star_count: i64) -> bool {
        let _ = star_count;
        tdutils::log_info!("StarManager::has_owned_star_count — [blocked: StarManager]");
        true
    }

    /// `td_->star_manager_->add_pending_owned_star_count(star_count,
    /// only_first)` (the C++ second arg names `need_only_first`).
    fn add_pending_owned_star_count(&mut self, star_count: i64, only_first: bool) {
        let _ = (star_count, only_first);
        tdutils::log_info!("StarManager::add_pending_owned_star_count — [blocked: StarManager]");
    }

    /// `dismiss_suggested_action(SuggestedAction{StarsSubscriptionLowBalance},
    /// Promise<Unit>())` (Payments.cpp:166-167) — the stars-topup purpose
    /// side effect.
    fn dismiss_suggested_action(&mut self) {
        tdutils::log_info!("dismiss_suggested_action — [blocked: SuggestedActionManager]");
    }

    // --- the BusinessConnectionManager seam ---
    /// `td_->business_connection_manager_->check_business_connection(
    /// connection_id, dialog_id)` (BusinessConnectionManager.h:50).
    fn check_business_connection(
        &mut self,
        _business_connection_id: &BusinessConnectionId,
        _dialog_id: DialogId,
    ) -> std::result::Result<(), Status> {
        tdutils::log_info!(
            "BusinessConnectionManager::check_business_connection — [blocked: BusinessConnections]"
        );
        std::result::Result::Ok(())
    }

    // --- the Files-plane seams ([blocked: M.12]) ---
    /// `get_web_document_photo(td_->file_manager_.get(), web_document,
    /// dialog_id)` (Photo.cpp:117-131); the default answers the empty
    /// photo (the payment product info builds photo-less until M.12).
    fn web_document_photo(
        &mut self,
        web_document: Option<telegram_api::WebDocument>,
        dialog_id: DialogId,
    ) -> Photo {
        let _ = (web_document, dialog_id);
        tdutils::log_info!("get_web_document_photo — [blocked: M.12 Files]");
        Photo::default()
    }

    /// `get_photo_object(td_->file_manager_.get(), photo)` (Photo.h:79).
    fn photo_object(&mut self, photo: &Photo) -> Option<td_api::Photo> {
        let _ = photo;
        tdutils::log_info!("get_photo_object — [blocked: M.12 Files]");
        None
    }

    /// `td_->file_manager_->from_persistent_id(url, FileType::Temp)`
    /// (InputInvoice.cpp:113-116) — renamed off the `from_*` convention
    /// to keep the clippy lints quiet.
    fn register_persistent_id(&mut self, url: &str) -> Result<crate::files::FileId> {
        let _ = url;
        tdutils::log_info!("FileManager::from_persistent_id — [blocked: M.12 Files]");
        Result::from_status(Status::error(400, "Can't register URL"))
    }

    /// `file_view.get_url()` behind `get_input_web_document`
    /// (InputInvoice.cpp:264).
    fn file_url(&mut self, file_id: crate::files::FileId) -> Option<String> {
        let _ = file_id;
        None
    }

    /// `extended_media_.get_input_media(td, nullptr, nullptr)`
    /// (InputInvoice.cpp:306-311) — the paid-media export tail; only
    /// reachable with a non-empty [`ExtendedMedia`].
    fn extended_media_input_media(&mut self) -> Option<telegram_api::InputMedia> {
        tdutils::log_info!("MessageExtendedMedia::get_input_media — [blocked: PaidMedia]");
        None
    }

    // --- the Premium seams ---
    /// `get_premium_gift_text(td, text)` (Premium.cpp:209-222) — the full
    /// port rides `get_formatted_text` + `get_input_text_with_entities`
    /// and lands with the Premium slice; `None` is the empty-text case.
    fn premium_gift_text(
        &mut self,
        text: Option<td_api::FormattedText>,
    ) -> Result<Option<telegram_api::TextWithEntities>> {
        let _ = text;
        tdutils::log_info!("get_premium_gift_text — [blocked: Premium slice]");
        Result::Ok(None)
    }

    // --- the paid-media constructor seams ---
    /// `MessageExtendedMedia::get_message_extended_media(td, paid_media,
    /// owner_dialog_id)` (InputInvoice.cpp:227-229) — the resolution of a
    /// paid media into the extended-media value ([blocked: PaidMedia]);
    /// the default answers the empty media.
    fn get_message_extended_media(
        &mut self,
        paid_media: Option<td_api::InputPaidMedia>,
        owner_dialog_id: DialogId,
    ) -> Result<ExtendedMedia> {
        let _ = (paid_media, owner_dialog_id);
        tdutils::log_info!(
            "MessageExtendedMedia::get_message_extended_media — [blocked: PaidMedia]"
        );
        Result::Ok(ExtendedMedia::default())
    }

    /// The paid-media caption parse behind
    /// `get_formatted_text(td, owner_dialog_id, caption, is_bot, true,
    /// false, false)` (InputInvoice.cpp:231-235) — the text-formatting
    /// wiring is the M.8.5/M.9 seam; the default answers the default text.
    fn get_extended_media_caption(
        &mut self,
        caption: Option<td_api::FormattedText>,
    ) -> Result<crate::message_entity::FormattedText> {
        let _ = caption;
        tdutils::log_info!(
            "get_formatted_text for the paid-media caption — [blocked: M.8.5 formatting]"
        );
        Result::Ok(crate::message_entity::FormattedText::default())
    }

    // --- the UpdatesManager tail seam ([blocked: net]) ---
    /// `td_->updates_manager_->on_get_updates(updates, promise)` with the
    /// payment-result continuation; the promise parks with the seam until
    /// the net step.
    fn on_get_updates(&mut self, updates: telegram_api::Updates, on_complete: Promise<()>) {
        let _ = (updates, on_complete);
        tdutils::log_info!("UpdatesManager::on_get_updates — [blocked: net]");
    }

    // --- the LinkManager seam ---
    /// `LinkManager::get_dialog_invite_link_hash(invite_link)`
    /// (LinkManager.cpp:4259-4275); the URL stack is Wave 2, so the
    /// default answers the empty hash (an invalid invite link).
    fn get_dialog_invite_link_hash(&mut self, invite_link: &str) -> String {
        let _ = invite_link;
        tdutils::log_info!(
            "LinkManager::get_dialog_invite_link_hash — [blocked: LinkManager Wave 2]"
        );
        String::new()
    }

    // --- the net query seams ([blocked: net]) ---
    /// `SetBotShippingAnswerQuery::send` (Payments.cpp:224-235); the
    /// promise rides the seam (the ResultHandler slot) until the net step.
    fn send_set_bot_shipping_answer_query(
        &mut self,
        shipping_query_id: i64,
        error_message: &str,
        shipping_options: Vec<telegram_api::ShippingOption>,
        promise: Promise<()>,
    ) {
        let _ = (shipping_options, promise);
        tdutils::log_info!(
            "Send messages.setBotShippingResults(query_id = {shipping_query_id}, \
             error = {error_message:?}) — [blocked: net]"
        );
    }

    /// `SetBotPreCheckoutAnswerQuery::send` (Payments.cpp:262-273).
    fn send_set_bot_pre_checkout_answer_query(
        &mut self,
        pre_checkout_query_id: i64,
        error_message: &str,
        promise: Promise<()>,
    ) {
        let _ = promise;
        tdutils::log_info!(
            "Send messages.setBotPrecheckoutResults(query_id = {pre_checkout_query_id}, \
             error = {error_message:?}) — [blocked: net]"
        );
    }

    /// `GetPaymentFormQuery::send` (Payments.cpp:500-509).
    fn send_get_payment_form_query(
        &mut self,
        input_invoice: telegram_api::InputInvoice,
        theme_parameters: Option<telegram_api::DataJSON>,
        promise: Promise<td_api::PaymentForm>,
    ) {
        let _ = (input_invoice, theme_parameters, promise);
        tdutils::log_info!("Send payments.getPaymentForm — [blocked: net]");
    }

    /// `ValidateRequestedInfoQuery::send` (Payments.cpp:615-624).
    fn send_validate_requested_info_query(
        &mut self,
        input_invoice: telegram_api::InputInvoice,
        requested_info: Option<telegram_api::PaymentRequestedInfo>,
        allow_save: bool,
        promise: Promise<td_api::ValidatedOrderInfo>,
    ) {
        let _ = (input_invoice, requested_info, allow_save, promise);
        tdutils::log_info!("Send payments.validateRequestedInfo — [blocked: net]");
    }

    /// `SendPaymentFormQuery::send` (Payments.cpp:655-675).
    #[allow(clippy::too_many_arguments)] // the C++ Query::send signature
    fn send_send_payment_form_query(
        &mut self,
        input_invoice: telegram_api::InputInvoice,
        payment_form_id: i64,
        order_info_id: &str,
        shipping_option_id: &str,
        input_credentials: telegram_api::InputPaymentCredentials,
        tip_amount: i64,
        promise: Promise<td_api::PaymentResult>,
    ) {
        let _ = (
            input_invoice,
            payment_form_id,
            order_info_id,
            shipping_option_id,
            input_credentials,
            tip_amount,
            promise,
        );
        tdutils::log_info!("Send payments.sendPaymentForm — [blocked: net]");
    }

    /// `SendStarPaymentFormQuery::send` (Payments.cpp:721-728).
    fn send_send_stars_form_query(
        &mut self,
        payment_form_id: i64,
        input_invoice: telegram_api::InputInvoice,
        promise: Promise<td_api::PaymentResult>,
    ) {
        let _ = (payment_form_id, input_invoice, promise);
        tdutils::log_info!("Send payments.sendStarsForm — [blocked: net]");
    }

    /// `GetPaymentReceiptQuery::send` (Payments.cpp:775-784).
    fn send_get_payment_receipt_query(
        &mut self,
        dialog_id: DialogId,
        server_message_id: ServerMessageId,
        promise: Promise<td_api::PaymentReceipt>,
    ) {
        let _ = (dialog_id, server_message_id, promise);
        tdutils::log_info!("Send payments.getPaymentReceipt — [blocked: net]");
    }

    /// `GetSavedInfoQuery::send` (Payments.cpp:867-869).
    fn send_get_saved_info_query(&mut self, promise: Promise<td_api::OrderInfo>) {
        let _ = promise;
        tdutils::log_info!("Send payments.getSavedInfo — [blocked: net]");
    }

    /// `ClearSavedInfoQuery::send` (Payments.cpp:894-898).
    fn send_clear_saved_info_query(
        &mut self,
        clear_credentials: bool,
        clear_order_info: bool,
        promise: Promise<()>,
    ) {
        let _ = (clear_credentials, clear_order_info, promise);
        tdutils::log_info!("Send payments.clearSavedInfo — [blocked: net]");
    }

    /// `ExportInvoiceQuery::send` (Payments.cpp:921-927); the
    /// business-connection invoke prefix and the DC-id hop are the net
    /// step's.
    fn send_export_invoice_query(
        &mut self,
        business_connection_id: &BusinessConnectionId,
        input_media_invoice: telegram_api::inputMediaInvoice,
        promise: Promise<String>,
    ) {
        let _ = (business_connection_id, input_media_invoice, promise);
        tdutils::log_info!("Send payments.exportInvoice — [blocked: net]");
    }

    /// `GetBankCardInfoQuery::send` (Payments.cpp:952-955) — the
    /// webfile-DC routing is the net step's.
    fn send_get_bank_card_info_query(
        &mut self,
        bank_card_number: &str,
        promise: Promise<td_api::BankCardInfo>,
    ) {
        let _ = (bank_card_number, promise);
        tdutils::log_info!("Send payments.getBankCardData — [blocked: net]");
    }

    /// `GetCollectibleInfoQuery::send` (Payments.cpp:983-986).
    fn send_get_collectible_info_query(
        &mut self,
        input_collectible: telegram_api::InputCollectible,
        promise: Promise<td_api::CollectibleItemInfo>,
    ) {
        let _ = (input_collectible, promise);
        tdutils::log_info!("Send fragment.getCollectibleInfo — [blocked: net]");
    }
}

/// `MessagesManager::InvoiceMessageInfo` (MessagesManager.h:1018-1021).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InvoiceMessageInfo {
    pub server_message_id: ServerMessageId,
    pub star_count: i64,
}

// ---------------------------------------------------------------------------
// the input-invoice info plane (Payments.cpp:46-215)
// ---------------------------------------------------------------------------

/// `InputInvoiceInfo` (Payments.cpp:48-52).
#[derive(Debug)]
struct InputInvoiceInfo {
    dialog_id: DialogId,
    input_invoice: Option<telegram_api::InputInvoice>,
    star_count: i64,
}

/// `get_input_invoice_info` (Payments.cpp:54-213).
fn get_input_invoice_info(
    env: &mut impl PaymentsEnv,
    input_invoice: Option<td_api::InputInvoice>,
) -> Result<InputInvoiceInfo> {
    let Some(input_invoice) = input_invoice else {
        return Result::from_status(Status::error(400, "Input invoice must be non-empty"));
    };

    let mut result = InputInvoiceInfo {
        dialog_id: DialogId::new(0),
        input_invoice: None,
        star_count: 0,
    };
    match input_invoice {
        td_api::InputInvoice::inputInvoiceMessage(invoice) => {
            let dialog_id = DialogId::new(invoice.chat_id);
            let message_id = MessageId::new(invoice.message_id);
            let invoice_message_info =
                match env.get_invoice_message_info(MessageFullId::new(dialog_id, message_id)) {
                    Result::Ok(info) => info,
                    Result::Err(status) => return Result::Err(status),
                };

            let Some(input_peer) = env.get_input_peer(dialog_id, AccessRights::Read) else {
                return Result::from_status(Status::error(400, "Can't access the chat"));
            };

            result.dialog_id = dialog_id;
            result.input_invoice = Some(telegram_api::InputInvoice::inputInvoiceMessage(
                telegram_api::inputInvoiceMessage {
                    peer: Some(Box::new(input_peer)),
                    msg_id: invoice_message_info.server_message_id.get(),
                },
            ));
            result.star_count = invoice_message_info.star_count;
        }
        td_api::InputInvoice::inputInvoiceName(invoice) => {
            result.input_invoice = Some(telegram_api::InputInvoice::inputInvoiceSlug(
                telegram_api::inputInvoiceSlug { slug: invoice.name },
            ));
        }
        td_api::InputInvoice::inputInvoiceTelegram(invoice) => {
            let Some(purpose) = invoice.purpose else {
                return Result::from_status(Status::error(400, "Purpose must be non-empty"));
            };
            match purpose {
                td_api::TelegramPaymentPurpose::telegramPaymentPurposePremiumGift(mut p) => {
                    let input_user = match env.get_input_user(UserId::new(p.user_id)) {
                        Result::Ok(input_user) => input_user,
                        Result::Err(status) => return Result::Err(status),
                    };
                    if let Result::Err(status) = check_payment_amount(&mut p.currency, p.amount) {
                        return Result::Err(status);
                    }
                    let text = match env.premium_gift_text(p.text.take()) {
                        Result::Ok(text) => text,
                        Result::Err(status) => return Result::Err(status),
                    };

                    if p.currency == "XTR" {
                        let mut flags = 0;
                        if text.is_some() {
                            flags |= INPUT_INVOICE_PREMIUM_GIFT_STARS_MESSAGE_MASK;
                        }
                        result.star_count = p.amount.max(0);
                        result.input_invoice =
                            Some(telegram_api::InputInvoice::inputInvoicePremiumGiftStars(
                                telegram_api::inputInvoicePremiumGiftStars {
                                    flags,
                                    user_id: Some(input_user),
                                    months: p.month_count,
                                    message: text,
                                },
                            ));
                        return Result::Ok(result);
                    }

                    let mut flags = 0;
                    if text.is_some() {
                        flags |= INPUT_STORE_PREMIUM_GIFT_CODE_MESSAGE_MASK;
                    }
                    let option = telegram_api::PremiumGiftCodeOption {
                        flags: 0,
                        users: 1,
                        months: p.month_count,
                        store_product: String::new(),
                        store_quantity: 0,
                        currency: p.currency.clone(),
                        amount: p.amount,
                    };
                    let purpose =
                        telegram_api::InputStorePaymentPurpose::inputStorePaymentPremiumGiftCode(
                            telegram_api::inputStorePaymentPremiumGiftCode {
                                flags,
                                users: vec![input_user],
                                boost_peer: None,
                                currency: p.currency,
                                amount: p.amount,
                                message: text,
                            },
                        );

                    result.input_invoice =
                        Some(telegram_api::InputInvoice::inputInvoicePremiumGiftCode(
                            telegram_api::inputInvoicePremiumGiftCode {
                                purpose: Some(purpose),
                                option: Some(option),
                            },
                        ));
                }
                td_api::TelegramPaymentPurpose::telegramPaymentPurposePremiumGiftCodes(mut p) => {
                    let user_ids: Vec<UserId> =
                        p.user_ids.iter().map(|&id| UserId::new(id)).collect();
                    let input_users = match env.get_input_users(&user_ids) {
                        Result::Ok(users) => users,
                        Result::Err(status) => return Result::Err(status),
                    };
                    if let Result::Err(status) = check_payment_amount(&mut p.currency, p.amount) {
                        return Result::Err(status);
                    }
                    let boosted_dialog_id = DialogId::new(p.boosted_chat_id);
                    let boost_input_peer = match get_boost_input_peer(env, boosted_dialog_id) {
                        Result::Ok(peer) => peer,
                        Result::Err(status) => return Result::Err(status),
                    };
                    let text = match env.premium_gift_text(p.text.take()) {
                        Result::Ok(text) => text,
                        Result::Err(status) => return Result::Err(status),
                    };

                    let mut flags = INPUT_STORE_PREMIUM_GIFT_CODE_BOOST_PEER_MASK;
                    if text.is_some() {
                        flags |= INPUT_STORE_PREMIUM_GIFT_CODE_MESSAGE_MASK;
                    }
                    let option = telegram_api::PremiumGiftCodeOption {
                        flags: 0,
                        users: input_users.len() as i32,
                        months: p.month_count,
                        store_product: String::new(),
                        store_quantity: 0,
                        currency: p.currency.clone(),
                        amount: p.amount,
                    };
                    let purpose =
                        telegram_api::InputStorePaymentPurpose::inputStorePaymentPremiumGiftCode(
                            telegram_api::inputStorePaymentPremiumGiftCode {
                                flags,
                                users: input_users,
                                boost_peer: Some(Box::new(boost_input_peer)),
                                currency: p.currency,
                                amount: p.amount,
                                message: text,
                            },
                        );

                    result.dialog_id = boosted_dialog_id;
                    result.input_invoice =
                        Some(telegram_api::InputInvoice::inputInvoicePremiumGiftCode(
                            telegram_api::inputInvoicePremiumGiftCode {
                                purpose: Some(purpose),
                                option: Some(option),
                            },
                        ));
                }
                td_api::TelegramPaymentPurpose::telegramPaymentPurposePremiumGiveaway(mut p) => {
                    if let Result::Err(status) = check_payment_amount(&mut p.currency, p.amount) {
                        return Result::Err(status);
                    }
                    let parameters = match GiveawayParameters::get_giveaway_parameters(
                        env,
                        p.parameters.as_ref(),
                    ) {
                        Result::Ok(parameters) => parameters,
                        Result::Err(status) => return Result::Err(status),
                    };
                    let option = telegram_api::PremiumGiftCodeOption {
                        flags: 0,
                        users: p.winner_count,
                        months: p.month_count,
                        store_product: String::new(),
                        store_quantity: 0,
                        currency: p.currency.clone(),
                        amount: p.amount,
                    };
                    let purpose = parameters.get_input_store_payment_premium_giveaway(
                        env,
                        &p.currency,
                        p.amount,
                    );
                    result.input_invoice =
                        Some(telegram_api::InputInvoice::inputInvoicePremiumGiftCode(
                            telegram_api::inputInvoicePremiumGiftCode {
                                purpose: Some(purpose),
                                option: Some(option),
                            },
                        ));
                }
                td_api::TelegramPaymentPurpose::telegramPaymentPurposeStars(mut p) => {
                    let mut flags = 0;
                    let mut spend_purpose_input_peer: Option<telegram_api::InputPeer> = None;
                    let spend_dialog_id = DialogId::new(p.chat_id);
                    if spend_dialog_id.is_valid() {
                        if let Result::Err(status) =
                            env.check_dialog_access(spend_dialog_id, "telegramPaymentPurposeStars")
                        {
                            return Result::Err(status);
                        }
                        spend_purpose_input_peer =
                            env.get_input_peer(spend_dialog_id, AccessRights::Read);
                        assert!(spend_purpose_input_peer.is_some());
                        flags |= INPUT_STORE_STARS_TOPUP_SPEND_PURPOSE_PEER_MASK;
                    }
                    if let Result::Err(status) = check_payment_amount(&mut p.currency, p.amount) {
                        return Result::Err(status);
                    }
                    // dismiss_suggested_action(StarsSubscriptionLowBalance,
                    // Promise<Unit>()) (Payments.cpp:166-167)
                    env.dismiss_suggested_action();
                    let purpose =
                        telegram_api::InputStorePaymentPurpose::inputStorePaymentStarsTopup(
                            telegram_api::inputStorePaymentStarsTopup {
                                flags,
                                stars: p.star_count,
                                currency: p.currency,
                                amount: p.amount,
                                spend_purpose_peer: spend_purpose_input_peer.map(Box::new),
                            },
                        );
                    result.input_invoice = Some(telegram_api::InputInvoice::inputInvoiceStars(
                        telegram_api::inputInvoiceStars {
                            purpose: Some(purpose),
                        },
                    ));
                }
                td_api::TelegramPaymentPurpose::telegramPaymentPurposeGiftedStars(p) => {
                    let input_user = match env.get_input_user(UserId::new(p.user_id)) {
                        Result::Ok(input_user) => input_user,
                        Result::Err(status) => return Result::Err(status),
                    };
                    let mut currency = p.currency;
                    if let Result::Err(status) = check_payment_amount(&mut currency, p.amount) {
                        return Result::Err(status);
                    }
                    let purpose =
                        telegram_api::InputStorePaymentPurpose::inputStorePaymentStarsGift(
                            telegram_api::inputStorePaymentStarsGift {
                                user_id: Some(input_user),
                                stars: p.star_count,
                                currency,
                                amount: p.amount,
                            },
                        );
                    result.input_invoice = Some(telegram_api::InputInvoice::inputInvoiceStars(
                        telegram_api::inputInvoiceStars {
                            purpose: Some(purpose),
                        },
                    ));
                }
                td_api::TelegramPaymentPurpose::telegramPaymentPurposeStarGiveaway(mut p) => {
                    if let Result::Err(status) = check_payment_amount(&mut p.currency, p.amount) {
                        return Result::Err(status);
                    }
                    let parameters = match GiveawayParameters::get_giveaway_parameters(
                        env,
                        p.parameters.as_ref(),
                    ) {
                        Result::Ok(parameters) => parameters,
                        Result::Err(status) => return Result::Err(status),
                    };
                    let purpose = parameters.get_input_store_payment_stars_giveaway(
                        env,
                        &p.currency,
                        p.amount,
                        p.winner_count,
                        p.star_count,
                    );
                    result.input_invoice = Some(telegram_api::InputInvoice::inputInvoiceStars(
                        telegram_api::inputInvoiceStars {
                            purpose: Some(purpose),
                        },
                    ));
                }
                td_api::TelegramPaymentPurpose::telegramPaymentPurposeJoinChat(p) => {
                    // DialogInviteLink::is_valid_invite_link
                    // (DialogInviteLink.cpp:97-102) is
                    // !get_dialog_invite_link_hash(...).empty(); the hash
                    // seam drives both
                    let hash = env.get_dialog_invite_link_hash(&p.invite_link);
                    if hash.is_empty() {
                        return Result::from_status(Status::error(400, "Invalid invite link"));
                    }
                    let Some(hash) = crate::misc::clean_input_string(&hash) else {
                        return Result::from_status(Status::error(400, "Invalid invite link"));
                    };
                    result.input_invoice = Some(
                        telegram_api::InputInvoice::inputInvoiceChatInviteSubscription(
                            telegram_api::inputInvoiceChatInviteSubscription { hash },
                        ),
                    );
                }
            }
        }
    }
    Result::Ok(result)
}

/// `get_boost_input_peer` (Premium.cpp:194-207).
fn get_boost_input_peer(
    env: &mut impl PaymentsEnv,
    dialog_id: DialogId,
) -> Result<telegram_api::InputPeer> {
    if !env.have_dialog_force(dialog_id, "get_boost_input_peer") {
        return Result::from_status(Status::error(400, "Chat to boost not found"));
    }
    if dialog_id.get_type() != crate::dialog_id::DialogType::Channel {
        return Result::from_status(Status::error(400, "Can't boost the chat"));
    }
    if !env
        .get_channel_status(dialog_id.get_channel_id())
        .is_administrator()
    {
        return Result::from_status(Status::error(400, "Not enough rights in the chat"));
    }
    let boost_input_peer = env.get_input_peer(dialog_id, AccessRights::Write);
    match boost_input_peer {
        Some(peer) => Result::Ok(peer),
        // the C++ CHECK(input_peer != nullptr)
        None => panic!("get_boost_input_peer has no input peer"),
    }
}

// ---------------------------------------------------------------------------
// the GiveawayParameters plane (GiveawayParameters.{h,cpp})
// ---------------------------------------------------------------------------

/// `td::GiveawayParameters` (GiveawayParameters.h:20-52) — the payment
/// plane's slice (the object getter and the dependencies land with their
/// consumers).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GiveawayParameters {
    boosted_channel_id: crate::chat_manager::ChannelId,
    additional_channel_ids: Vec<crate::chat_manager::ChannelId>,
    only_new_subscribers: bool,
    winners_are_visible: bool,
    date: i32,
    country_codes: Vec<String>,
    prize_description: String,
}

impl GiveawayParameters {
    /// `GiveawayParameters::get_boosted_channel_id`
    /// (GiveawayParameters.cpp:22-33).
    fn get_boosted_channel_id(
        env: &mut impl PaymentsEnv,
        dialog_id: DialogId,
    ) -> Result<crate::chat_manager::ChannelId> {
        if !env.have_dialog_force(dialog_id, "get_boosted_channel_id") {
            return Result::from_status(Status::error(400, "Chat to boost not found"));
        }
        if dialog_id.get_type() != crate::dialog_id::DialogType::Channel {
            return Result::from_status(Status::error(400, "Can't boost the chat"));
        }
        let channel_id = dialog_id.get_channel_id();
        let status = env.get_channel_status(channel_id);
        let enough_rights = if env.is_broadcast_channel(channel_id) {
            status.can_post_messages()
        } else {
            status.is_administrator()
        };
        if !enough_rights {
            return Result::from_status(Status::error(400, "Not enough rights in the chat"));
        }
        Result::Ok(channel_id)
    }

    /// `GiveawayParameters::get_giveaway_parameters`
    /// (GiveawayParameters.cpp:39-80).
    fn get_giveaway_parameters(
        env: &mut impl PaymentsEnv,
        parameters: Option<&td_api::GiveawayParameters>,
    ) -> Result<GiveawayParameters> {
        let Some(parameters) = parameters else {
            return Result::from_status(Status::error(
                400,
                "Giveaway parameters must be non-empty",
            ));
        };
        let boosted_channel_id =
            match Self::get_boosted_channel_id(env, DialogId::new(parameters.boosted_chat_id)) {
                Result::Ok(channel_id) => channel_id,
                Result::Err(status) => return Result::Err(status),
            };
        let mut additional_channel_ids = Vec::new();
        for &additional_chat_id in &parameters.additional_chat_ids {
            match Self::get_boosted_channel_id(env, DialogId::new(additional_chat_id)) {
                Result::Ok(channel_id) => additional_channel_ids.push(channel_id),
                Result::Err(status) => return Result::Err(status),
            }
        }
        if additional_channel_ids.len() as i64
            > env.get_option_integer("giveaway_additional_chat_count_max")
        {
            return Result::from_status(Status::error(400, "Too many additional chats specified"));
        }
        if parameters.winners_selection_date < env.get_unix_time() {
            return Result::from_status(Status::error(400, "Giveaway date is in the past"));
        }
        for country_code in &parameters.country_codes {
            let bytes = country_code.as_bytes();
            if bytes.len() != 2 || !bytes[0].is_ascii_uppercase() || !bytes[1].is_ascii_uppercase()
            {
                return Result::from_status(Status::error(400, "Invalid country code specified"));
            }
        }
        if parameters.country_codes.len() as i64
            > env.get_option_integer("giveaway_country_count_max")
        {
            return Result::from_status(Status::error(400, "Too many countries specified"));
        }
        let Some(prize_description) =
            crate::misc::clean_input_string(&parameters.prize_description)
        else {
            return Result::from_status(Status::error(400, "Strings must be encoded in UTF-8"));
        };
        Result::Ok(GiveawayParameters {
            boosted_channel_id,
            additional_channel_ids,
            only_new_subscribers: parameters.only_new_members,
            winners_are_visible: parameters.has_public_winners,
            date: parameters.winners_selection_date,
            country_codes: parameters.country_codes.clone(),
            prize_description,
        })
    }

    /// `GiveawayParameters::get_input_store_payment_premium_giveaway`
    /// (GiveawayParameters.cpp:90-121).
    fn get_input_store_payment_premium_giveaway(
        &self,
        env: &mut impl PaymentsEnv,
        currency: &str,
        amount: i64,
    ) -> telegram_api::InputStorePaymentPurpose {
        // the C++ do/while loop over Random::secure_int64
        let mut random_id = tdutils::random::secure_int64();
        while random_id == 0 {
            random_id = tdutils::random::secure_int64();
        }

        let boost_input_peer = env.get_input_peer(
            DialogId::from_channel_id(self.boosted_channel_id),
            AccessRights::Write,
        );
        assert!(boost_input_peer.is_some());
        let boost_input_peer = boost_input_peer.expect("checked above");

        let mut additional_input_peers: Vec<Box<telegram_api::InputPeer>> = Vec::new();
        for &additional_channel_id in &self.additional_channel_ids {
            let input_peer = env.get_input_peer(
                DialogId::from_channel_id(additional_channel_id),
                AccessRights::Write,
            );
            assert!(input_peer.is_some());
            additional_input_peers.push(Box::new(input_peer.expect("checked above")));
        }

        let mut flags = 0;
        if !additional_input_peers.is_empty() {
            flags |= INPUT_STORE_PREMIUM_GIVEAWAY_ADDITIONAL_PEERS_MASK;
        }
        if !self.country_codes.is_empty() {
            flags |= INPUT_STORE_PREMIUM_GIVEAWAY_COUNTRIES_ISO2_MASK;
        }
        if !self.prize_description.is_empty() {
            flags |= INPUT_STORE_PREMIUM_GIVEAWAY_PRIZE_DESCRIPTION_MASK;
        }
        telegram_api::InputStorePaymentPurpose::inputStorePaymentPremiumGiveaway(
            telegram_api::inputStorePaymentPremiumGiveaway {
                flags,
                only_new_subscribers: self.only_new_subscribers,
                winners_are_visible: self.winners_are_visible,
                boost_peer: Some(Box::new(boost_input_peer)),
                additional_peers: additional_input_peers,
                countries_iso2: self.country_codes.clone(),
                prize_description: self.prize_description.clone(),
                random_id,
                until_date: self.date,
                currency: currency.to_owned(),
                amount,
            },
        )
    }

    /// `GiveawayParameters::get_input_store_payment_stars_giveaway`
    /// (GiveawayParameters.cpp:123-156).
    fn get_input_store_payment_stars_giveaway(
        &self,
        env: &mut impl PaymentsEnv,
        currency: &str,
        amount: i64,
        user_count: i32,
        star_count: i64,
    ) -> telegram_api::InputStorePaymentPurpose {
        let mut random_id = tdutils::random::secure_int64();
        while random_id == 0 {
            random_id = tdutils::random::secure_int64();
        }

        let boost_input_peer = env.get_input_peer(
            DialogId::from_channel_id(self.boosted_channel_id),
            AccessRights::Write,
        );
        assert!(boost_input_peer.is_some());
        let boost_input_peer = boost_input_peer.expect("checked above");

        let mut additional_input_peers: Vec<Box<telegram_api::InputPeer>> = Vec::new();
        for &additional_channel_id in &self.additional_channel_ids {
            let input_peer = env.get_input_peer(
                DialogId::from_channel_id(additional_channel_id),
                AccessRights::Write,
            );
            assert!(input_peer.is_some());
            additional_input_peers.push(Box::new(input_peer.expect("checked above")));
        }

        let mut flags = 0;
        if !additional_input_peers.is_empty() {
            flags |= INPUT_STORE_PREMIUM_GIVEAWAY_ADDITIONAL_PEERS_MASK;
        }
        if !self.country_codes.is_empty() {
            flags |= INPUT_STORE_PREMIUM_GIVEAWAY_COUNTRIES_ISO2_MASK;
        }
        if !self.prize_description.is_empty() {
            flags |= INPUT_STORE_PREMIUM_GIVEAWAY_PRIZE_DESCRIPTION_MASK;
        }
        telegram_api::InputStorePaymentPurpose::inputStorePaymentStarsGiveaway(
            telegram_api::inputStorePaymentStarsGiveaway {
                flags,
                only_new_subscribers: self.only_new_subscribers,
                winners_are_visible: self.winners_are_visible,
                stars: star_count,
                boost_peer: Some(Box::new(boost_input_peer)),
                additional_peers: additional_input_peers,
                countries_iso2: self.country_codes.clone(),
                prize_description: self.prize_description.clone(),
                random_id,
                until_date: self.date,
                currency: currency.to_owned(),
                amount,
                users: user_count,
            },
        )
    }
}

// ---------------------------------------------------------------------------
// `get_product_info_object` (InputInvoice.cpp:400-408)
// ---------------------------------------------------------------------------

/// `get_product_info_object` (InputInvoice.cpp:400-408). The description
/// entities are built manager-less (see the module divergences); the
/// photo object rides the [`PaymentsEnv::photo_object`] seam.
fn get_product_info_object(
    env: &mut impl PaymentsEnv,
    title: &str,
    description: &str,
    photo: &Photo,
) -> td_api::ProductInfo {
    let formatted_description = crate::message_entity::FormattedText {
        text: description.to_owned(),
        entities: crate::message_entity::find_entities(description, true, true),
    };
    td_api::ProductInfo {
        title: title.to_owned(),
        description: Some(crate::message_entity::get_formatted_text_object(
            None::<&mut ()>,
            &formatted_description,
            true,
            -1,
        )),
        photo: env.photo_object(photo),
    }
}

// ---------------------------------------------------------------------------
// `process_input_message_invoice` (InputInvoice.cpp:80-276)
// ---------------------------------------------------------------------------

/// The `clean_input_string` TRY ladder of
/// `process_input_message_invoice` (InputInvoice.cpp:90-123).
fn clean_invoice_string(s: &str, error: &str) -> Result<String> {
    match crate::misc::clean_input_string(s) {
        Some(cleaned) => Result::Ok(cleaned),
        None => Result::from_status(Status::error(400, error)),
    }
}

/// `InputInvoice::process_input_message_invoice` (InputInvoice.cpp:80-276)
/// for the `inputMessageInvoice` call shape (the caller CHECKs the type).
pub fn process_input_message_invoice(
    env: &mut impl PaymentsEnv,
    invoice: td_api::inputMessageInvoice,
    owner_dialog_id: DialogId,
) -> Result<InputInvoice> {
    let td_api::inputMessageInvoice {
        invoice: input_invoice_data,
        title,
        description,
        photo_url,
        photo_size,
        photo_width,
        photo_height,
        payload,
        provider_token,
        provider_data,
        start_parameter,
        paid_media,
        paid_media_caption,
    } = invoice;
    let Some(input_invoice_data) = input_invoice_data else {
        return Result::from_status(Status::error(400, "Invoice must be non-empty"));
    };

    let title = match clean_invoice_string(&title, "Invoice title must be encoded in UTF-8") {
        Result::Ok(cleaned) => cleaned,
        Result::Err(status) => return Result::Err(status),
    };
    let description =
        match clean_invoice_string(&description, "Invoice description must be encoded in UTF-8") {
            Result::Ok(cleaned) => cleaned,
            Result::Err(status) => return Result::Err(status),
        };
    let photo_url =
        match clean_invoice_string(&photo_url, "Invoice photo URL must be encoded in UTF-8") {
            Result::Ok(cleaned) => cleaned,
            Result::Err(status) => return Result::Err(status),
        };
    let start_parameter = match clean_invoice_string(
        &start_parameter,
        "Invoice bot start parameter must be encoded in UTF-8",
    ) {
        Result::Ok(cleaned) => cleaned,
        Result::Err(status) => return Result::Err(status),
    };
    let provider_token = match clean_invoice_string(
        &provider_token,
        "Invoice provider token must be encoded in UTF-8",
    ) {
        Result::Ok(cleaned) => cleaned,
        Result::Err(status) => return Result::Err(status),
    };
    let provider_data = match clean_invoice_string(
        &provider_data,
        "Invoice provider data must be encoded in UTF-8",
    ) {
        Result::Ok(cleaned) => cleaned,
        Result::Err(status) => return Result::Err(status),
    };
    let currency = match clean_invoice_string(
        &input_invoice_data.currency,
        "Invoice currency must be encoded in UTF-8",
    ) {
        Result::Ok(cleaned) => cleaned,
        Result::Err(status) => return Result::Err(status),
    };
    let recurring_payment_terms_of_service_url = match clean_invoice_string(
        &input_invoice_data.recurring_payment_terms_of_service_url,
        "Invoice terms of service URL must be encoded in UTF-8",
    ) {
        Result::Ok(cleaned) => cleaned,
        Result::Err(status) => return Result::Err(status),
    };
    let terms_of_service_url = match clean_invoice_string(
        &input_invoice_data.terms_of_service_url,
        "Invoice terms of service URL must be encoded in UTF-8",
    ) {
        Result::Ok(cleaned) => cleaned,
        Result::Err(status) => return Result::Err(status),
    };

    let mut result = InputInvoice {
        title,
        description,
        ..InputInvoice::default()
    };

    // the photo URL registration (InputInvoice.cpp:111-133): the URL
    // parse is the tdutils port's; the persistent-id registration is the
    // Files seam
    let mut registered_file_id = crate::files::FileId::default();
    match tdutils::http_url::parse_url(&photo_url) {
        Result::Ok(http_url) => {
            let url = http_url.get_url();
            match env.register_persistent_id(&url) {
                Result::Ok(invoice_file_id) => registered_file_id = invoice_file_id,
                Result::Err(_) => {
                    tdutils::log_info!("Can't register URL {url}");
                }
            }
        }
        Result::Err(_) => {
            if !photo_url.is_empty() {
                tdutils::log_info!("Can't register URL {photo_url}");
            }
        }
    }
    if registered_file_id.is_valid() {
        result.photo = Photo {
            id: 0,
            photos: vec![crate::photo::PhotoSize {
                size_type: b'n' as crate::photo::PhotoSizeType,
                dimensions: crate::photo::get_dimensions(photo_width, photo_height, ""),
                size: photo_size, // TODO use invoice_file_id size
                file_id: registered_file_id,
                progressive_sizes: Vec::new(),
            }],
            ..Photo::default()
        };
    }
    result.start_parameter = start_parameter;

    let price_parts = match LabeledPricePart::get_labeled_price_parts(
        input_invoice_data.price_parts,
        Some(&mut result.total_amount),
    ) {
        Result::Ok(parts) => parts,
        Result::Err(status) => return Result::Err(status),
    };
    result.invoice.price_parts = price_parts;
    result.invoice.subscription_period = input_invoice_data.subscription_period.max(0);

    if input_invoice_data.max_tip_amount < 0
        || !crate::misc::check_currency_amount(input_invoice_data.max_tip_amount)
    {
        return Result::from_status(Status::error(
            400,
            "Invalid max_tip_amount of the currency specified",
        ));
    }
    for &tip_amount in &input_invoice_data.suggested_tip_amounts {
        if tip_amount <= 0 {
            return Result::from_status(Status::error(
                400,
                "Suggested tip amount must be positive",
            ));
        }
        if tip_amount > input_invoice_data.max_tip_amount {
            return Result::from_status(Status::error(
                400,
                "Suggested tip amount can't be bigger than max_tip_amount",
            ));
        }
    }
    if input_invoice_data.suggested_tip_amounts.len() > 4 {
        return Result::from_status(Status::error(
            400,
            "There can be at most 4 suggested tip amounts",
        ));
    }

    result.invoice.currency = currency;
    result.invoice.max_tip_amount = input_invoice_data.max_tip_amount;
    result.invoice.suggested_tip_amounts = input_invoice_data.suggested_tip_amounts;
    result.invoice.recurring_payment_terms_of_service_url = recurring_payment_terms_of_service_url;
    result.invoice.terms_of_service_url = terms_of_service_url;
    result.invoice.is_test = input_invoice_data.is_test;
    result.invoice.need_name = input_invoice_data.need_name;
    result.invoice.need_phone_number = input_invoice_data.need_phone_number;
    result.invoice.need_email_address = input_invoice_data.need_email_address;
    result.invoice.need_shipping_address = input_invoice_data.need_shipping_address;
    result.invoice.send_phone_number_to_provider = input_invoice_data.send_phone_number_to_provider;
    result.invoice.send_email_address_to_provider =
        input_invoice_data.send_email_address_to_provider;
    result.invoice.is_flexible = input_invoice_data.is_flexible;
    if result.invoice.send_phone_number_to_provider {
        result.invoice.need_phone_number = true;
    }
    if result.invoice.send_email_address_to_provider {
        result.invoice.need_email_address = true;
    }
    if result.invoice.is_flexible {
        result.invoice.need_shipping_address = true;
    }

    result.payload = payload;
    result.provider_token = provider_token;
    result.provider_data = provider_data;

    result.extended_media =
        match env.get_message_extended_media(paid_media.clone(), owner_dialog_id) {
            Result::Ok(extended_media) => extended_media,
            Result::Err(status) => return Result::Err(status),
        };
    if !result.extended_media.is_empty() {
        // the C++ reads is_bot from the auth state here
        // (InputInvoice.cpp:230-236); the caption seam carries it
        result.extended_media_caption = match env.get_extended_media_caption(paid_media_caption) {
            Result::Ok(caption) => caption,
            Result::Err(status) => return Result::Err(status),
        };
    }

    Result::Ok(result)
}

// ---------------------------------------------------------------------------
// the request flows (Payments.cpp:1013-1234)
// ---------------------------------------------------------------------------

/// Complete a promise with the C++ `promise.set_error(code, message)`.
fn promise_error<T: Send + 'static>(mut promise: Promise<T>, code: i32, message: &str) {
    promise.set_error(Status::error(code, message));
}

/// `answer_shipping_query` (Payments.cpp:1013-1040).
pub fn answer_shipping_query(
    env: &mut impl PaymentsEnv,
    shipping_query_id: i64,
    shipping_options: Vec<td_api::ShippingOption>,
    error_message: &str,
    mut promise: Promise<()>,
) {
    let mut options: Vec<telegram_api::ShippingOption> = Vec::new();
    for mut option in shipping_options {
        let Some(id) = crate::misc::clean_input_string(&option.id) else {
            return promise_error(
                promise,
                400,
                "Shipping option identifier must be encoded in UTF-8",
            );
        };
        let Some(title) = crate::misc::clean_input_string(&option.title) else {
            return promise_error(
                promise,
                400,
                "Shipping option title must be encoded in UTF-8",
            );
        };

        let labeled_price_parts = match LabeledPricePart::get_labeled_price_parts(
            std::mem::take(&mut option.price_parts),
            None,
        ) {
            Result::Ok(parts) => parts,
            Result::Err(status) => return promise.set_error(status),
        };
        options.push(telegram_api::ShippingOption {
            id,
            title,
            prices: LabeledPricePart::get_input_labeled_prices(&labeled_price_parts),
        });
    }
    if options.is_empty() {
        return promise_error(promise, 400, "There must be at least one shipping option");
    }

    env.send_set_bot_shipping_answer_query(shipping_query_id, error_message, options, promise);
}

/// `answer_pre_checkout_query` (Payments.cpp:1042-1045).
pub fn answer_pre_checkout_query(
    env: &mut impl PaymentsEnv,
    pre_checkout_query_id: i64,
    error_message: &str,
    promise: Promise<()>,
) {
    env.send_set_bot_pre_checkout_answer_query(pre_checkout_query_id, error_message, promise);
}

/// `get_payment_form` (Payments.cpp:1047-1059).
pub fn get_payment_form(
    env: &mut impl PaymentsEnv,
    input_invoice: Option<td_api::InputInvoice>,
    theme: Option<&td_api::ThemeParameters>,
    mut promise: Promise<td_api::PaymentForm>,
) {
    let input_invoice_info = match get_input_invoice_info(env, input_invoice) {
        Result::Ok(info) => info,
        Result::Err(status) => return promise.set_error(status),
    };

    let theme_parameters = theme.map(|theme| telegram_api::DataJSON {
        data: get_theme_parameters_json_string(theme),
    });
    env.send_get_payment_form_query(
        input_invoice_info
            .input_invoice
            .expect("get_input_invoice_info always builds the wire invoice"),
        theme_parameters,
        promise,
    );
}

/// `validate_order_info` (Payments.cpp:1061-1100).
pub fn validate_order_info(
    env: &mut impl PaymentsEnv,
    input_invoice: Option<td_api::InputInvoice>,
    order_info: Option<td_api::OrderInfo>,
    allow_save: bool,
    mut promise: Promise<td_api::ValidatedOrderInfo>,
) {
    let input_invoice_info = match get_input_invoice_info(env, input_invoice) {
        Result::Ok(info) => info,
        Result::Err(status) => return promise.set_error(status),
    };

    if let Some(order_info) = order_info.as_ref() {
        let clean = |s: &str, error: &str| -> Result<()> {
            if crate::misc::clean_input_string(s).is_some() {
                Result::Ok(())
            } else {
                Result::from_status(Status::error(400, error))
            }
        };
        if let Result::Err(status) = clean(&order_info.name, "Name must be encoded in UTF-8") {
            return promise.set_error(status);
        }
        if let Result::Err(status) = clean(
            &order_info.phone_number,
            "Phone number must be encoded in UTF-8",
        ) {
            return promise.set_error(status);
        }
        if let Result::Err(status) = clean(
            &order_info.email_address,
            "Email address must be encoded in UTF-8",
        ) {
            return promise.set_error(status);
        }
        if let Some(shipping_address) = order_info.shipping_address.as_ref() {
            if let Result::Err(status) = clean(
                &shipping_address.country_code,
                "Country code must be encoded in UTF-8",
            ) {
                return promise.set_error(status);
            }
            if let Result::Err(status) =
                clean(&shipping_address.state, "State must be encoded in UTF-8")
            {
                return promise.set_error(status);
            }
            if let Result::Err(status) =
                clean(&shipping_address.city, "City must be encoded in UTF-8")
            {
                return promise.set_error(status);
            }
            if let Result::Err(status) = clean(
                &shipping_address.street_line1,
                "Street address must be encoded in UTF-8",
            ) {
                return promise.set_error(status);
            }
            if let Result::Err(status) = clean(
                &shipping_address.street_line2,
                "Street address must be encoded in UTF-8",
            ) {
                return promise.set_error(status);
            }
            if let Result::Err(status) = clean(
                &shipping_address.postal_code,
                "Postal code must be encoded in UTF-8",
            ) {
                return promise.set_error(status);
            }
        }
    }

    // the ValidateRequestedInfoQuery::send null-info normalization
    // (Payments.cpp:619-621)
    let requested_info = match convert_order_info_to_telegram(order_info) {
        Some(info) => Some(info),
        None => Some(telegram_api::PaymentRequestedInfo::default()),
    };
    env.send_validate_requested_info_query(
        input_invoice_info
            .input_invoice
            .expect("get_input_invoice_info always builds the wire invoice"),
        requested_info,
        allow_save,
        promise,
    );
}

/// `send_payment_form` (Payments.cpp:1102-1162).
#[allow(clippy::too_many_arguments)] // the C++ signature
pub fn send_payment_form(
    env: &mut impl PaymentsEnv,
    input_invoice: Option<td_api::InputInvoice>,
    payment_form_id: i64,
    order_info_id: &str,
    shipping_option_id: &str,
    credentials: Option<td_api::InputCredentials>,
    tip_amount: i64,
    mut promise: Promise<td_api::PaymentResult>,
) {
    let input_invoice_info = match get_input_invoice_info(env, input_invoice) {
        Result::Ok(info) => info,
        Result::Err(status) => return promise.set_error(status),
    };

    let Some(credentials) = credentials else {
        if tip_amount != 0 || !order_info_id.is_empty() || !shipping_option_id.is_empty() {
            return promise_error(promise, 400, "Invalid payment form parameters specified");
        }
        if !env.has_owned_star_count(input_invoice_info.star_count) {
            return promise_error(
                promise,
                400,
                "Have not enough Telegram Stars to complete payment",
            );
        }
        // SendStarPaymentFormQuery::send (Payments.cpp:721-728): the
        // pending-stars bookkeeping rides the send
        env.add_pending_owned_star_count(-input_invoice_info.star_count, false);
        let input_invoice = input_invoice_info
            .input_invoice
            .expect("get_input_invoice_info always builds the wire invoice");
        env.send_send_stars_form_query(payment_form_id, input_invoice, promise);
        return;
    };

    let input_credentials: telegram_api::InputPaymentCredentials = match credentials {
        td_api::InputCredentials::inputCredentialsSaved(credentials_saved) => {
            let credentials_id = credentials_saved.saved_credentials_id;
            let Some(credentials_id) = crate::misc::clean_input_string(&credentials_id) else {
                return promise_error(
                    promise,
                    400,
                    "Credentials identifier must be encoded in UTF-8",
                );
            };
            let temp_password_state = get_temp_password_state_sync(env);
            if !temp_password_state.has_temp_password {
                return promise_error(
                    promise,
                    400,
                    "Temporary password required to use saved credentials",
                );
            }

            telegram_api::InputPaymentCredentials::inputPaymentCredentialsSaved(
                telegram_api::inputPaymentCredentialsSaved {
                    id: credentials_id,
                    tmp_password: temp_password_state.temp_password.into_bytes(),
                },
            )
        }
        td_api::InputCredentials::inputCredentialsNew(credentials_new) => {
            telegram_api::InputPaymentCredentials::inputPaymentCredentials(
                telegram_api::inputPaymentCredentials {
                    flags: 0,
                    save: credentials_new.allow_save,
                    data: Some(telegram_api::DataJSON {
                        data: credentials_new.data,
                    }),
                },
            )
        }
        td_api::InputCredentials::inputCredentialsGooglePay(credentials_google_pay) => {
            telegram_api::InputPaymentCredentials::inputPaymentCredentialsGooglePay(
                telegram_api::inputPaymentCredentialsGooglePay {
                    payment_token: Some(telegram_api::DataJSON {
                        data: credentials_google_pay.data,
                    }),
                },
            )
        }
        td_api::InputCredentials::inputCredentialsApplePay(credentials_apple_pay) => {
            telegram_api::InputPaymentCredentials::inputPaymentCredentialsApplePay(
                telegram_api::inputPaymentCredentialsApplePay {
                    payment_data: Some(telegram_api::DataJSON {
                        data: credentials_apple_pay.data,
                    }),
                },
            )
        }
    };

    let input_invoice = input_invoice_info
        .input_invoice
        .expect("get_input_invoice_info always builds the wire invoice");
    env.send_send_payment_form_query(
        input_invoice,
        payment_form_id,
        order_info_id,
        shipping_option_id,
        input_credentials,
        tip_amount,
        promise,
    );
}

/// `get_payment_receipt` (Payments.cpp:1164-1170).
pub fn get_payment_receipt(
    env: &mut impl PaymentsEnv,
    message_full_id: MessageFullId,
    mut promise: Promise<td_api::PaymentReceipt>,
) {
    let server_message_id = match env.get_payment_successful_message_id(message_full_id) {
        Result::Ok(server_message_id) => server_message_id,
        Result::Err(status) => return promise.set_error(status),
    };
    env.send_get_payment_receipt_query(message_full_id.get_dialog_id(), server_message_id, promise);
}

/// `get_saved_order_info` (Payments.cpp:1172-1174).
pub fn get_saved_order_info(env: &mut impl PaymentsEnv, promise: Promise<td_api::OrderInfo>) {
    env.send_get_saved_info_query(promise);
}

/// `delete_saved_order_info` (Payments.cpp:1176-1178).
pub fn delete_saved_order_info(env: &mut impl PaymentsEnv, promise: Promise<()>) {
    env.send_clear_saved_info_query(false, true, promise);
}

/// `delete_saved_credentials` (Payments.cpp:1180-1182).
pub fn delete_saved_credentials(env: &mut impl PaymentsEnv, promise: Promise<()>) {
    env.send_clear_saved_info_query(true, false, promise);
}

/// `export_invoice` (Payments.cpp:1184-1200).
pub fn export_invoice(
    env: &mut impl PaymentsEnv,
    business_connection_id: BusinessConnectionId,
    invoice: td_api::InputMessageContent,
    mut promise: Promise<String>,
) {
    let td_api::InputMessageContent::inputMessageInvoice(invoice) = invoice else {
        // the C++ CHECK(input_message_content->get_id() ==
        // inputMessageInvoice::ID) is unreachable through the request
        // arm; the non-invoice content is the null-invoice error shape
        return promise_error(promise, 400, "Invoice must be non-empty");
    };
    let input_invoice = match process_input_message_invoice(env, invoice, DialogId::new(0)) {
        Result::Ok(input_invoice) => input_invoice,
        Result::Err(status) => return promise.set_error(status),
    };
    if business_connection_id.is_valid()
        && let std::result::Result::Err(status) =
            env.check_business_connection(&business_connection_id, env.get_my_dialog_id())
    {
        return promise.set_error(status);
    }

    let input_media = input_invoice
        .get_input_media_invoice(env)
        .expect("invoice media is always buildable for a photo-less invoice");
    env.send_export_invoice_query(&business_connection_id, input_media, promise);
}

/// `get_bank_card_info` (Payments.cpp:1202-1205).
pub fn get_bank_card_info(
    env: &mut impl PaymentsEnv,
    bank_card_number: &str,
    promise: Promise<td_api::BankCardInfo>,
) {
    env.send_get_bank_card_info_query(bank_card_number, promise);
}

/// `get_collectible_info` (Payments.cpp:1207-1234).
pub fn get_collectible_info(
    env: &mut impl PaymentsEnv,
    r#type: Option<td_api::CollectibleItemType>,
    promise: Promise<td_api::CollectibleItemInfo>,
) {
    let Some(r#type) = r#type else {
        return promise_error(promise, 400, "Item type must be non-empty");
    };
    match r#type {
        td_api::CollectibleItemType::collectibleItemTypeUsername(username) => {
            let Some(username) = crate::misc::clean_input_string(&username.username) else {
                return promise_error(promise, 400, "Username must be encoded in UTF-8");
            };
            env.send_get_collectible_info_query(
                telegram_api::InputCollectible::inputCollectibleUsername(
                    telegram_api::inputCollectibleUsername { username },
                ),
                promise,
            );
        }
        td_api::CollectibleItemType::collectibleItemTypePhoneNumber(phone_number) => {
            let Some(phone_number) = crate::misc::clean_input_string(&phone_number.phone_number)
            else {
                return promise_error(promise, 400, "Phone number must be encoded in UTF-8");
            };
            env.send_get_collectible_info_query(
                telegram_api::InputCollectible::inputCollectiblePhone(
                    telegram_api::inputCollectiblePhone {
                        phone: phone_number,
                    },
                ),
                promise,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// the ResultHandler on_result halves (Payments.cpp:217-1011) — driven by
// the net integration; unit-tested here
// ---------------------------------------------------------------------------

/// `GetPaymentFormQuery::on_result` (Payments.cpp:511-603) for the plain
/// `payments_paymentForm` answer; the fetch error/on_error collapse is
/// the caller's `Result`.
pub fn on_get_payment_form_result(
    env: &mut impl PaymentsEnv,
    dialog_id: DialogId,
    payment_form: telegram_api::payments_paymentForm,
    mut promise: Promise<td_api::PaymentForm>,
) {
    env.on_get_users(payment_form.users, "GetPaymentFormQuery 1");

    let payments_provider_user_id = UserId::new(payment_form.provider_id);
    if !payments_provider_user_id.is_valid() {
        tdutils::log_error!(
            "Receive invalid payments provider {}",
            payments_provider_user_id.get()
        );
        return promise_error(promise, 500, "Receive invalid payments provider identifier");
    }
    let seller_bot_user_id = UserId::new(payment_form.bot_id);
    if !seller_bot_user_id.is_valid() {
        tdutils::log_error!("Receive invalid seller {}", seller_bot_user_id.get());
        return promise_error(promise, 500, "Receive invalid seller identifier");
    }
    let can_save_credentials = payment_form.can_save_credentials;
    let need_password = payment_form.password_missing;
    let photo = env.web_document_photo(payment_form.photo, dialog_id);
    let invoice = payment_form
        .invoice
        .expect("payments_paymentForm carries its invoice");
    let payment_provider = convert_payment_provider(
        &payment_form.native_provider,
        payment_form.native_params,
        invoice.test,
    )
    .unwrap_or_else(|| {
        td_api::PaymentProvider::paymentProviderOther(td_api::paymentProviderOther {
            url: payment_form.url.clone(),
        })
    });
    let additional_payment_options: Vec<td_api::PaymentOption> = payment_form
        .additional_methods
        .iter()
        .map(|method| td_api::PaymentOption {
            title: method.title.clone(),
            url: method.url.clone(),
        })
        .collect();
    let r#type = td_api::PaymentFormType::paymentFormTypeRegular(td_api::paymentFormTypeRegular {
        invoice: Some(convert_invoice(invoice)),
        payment_provider_user_id: env
            .get_user_id_object(payments_provider_user_id, "paymentForm provider"),
        payment_provider: Some(payment_provider),
        additional_payment_options,
        saved_order_info: convert_order_info_to_td(payment_form.saved_info),
        saved_credentials: convert_saved_credentials(payment_form.saved_credentials),
        can_save_credentials,
        need_password,
    });
    let seller_user_id = env.get_user_id_object(seller_bot_user_id, "paymentForm seller");
    let product_info =
        get_product_info_object(env, &payment_form.title, &payment_form.description, &photo);
    promise.set_value(td_api::PaymentForm {
        id: payment_form.form_id,
        r#type: Some(r#type),
        seller_bot_user_id: seller_user_id,
        product_info: Some(product_info),
    });
}

/// `GetPaymentFormQuery::on_result` (Payments.cpp:561-590) for the
/// `payments_paymentFormStars` answer.
pub fn on_get_payment_form_stars_result(
    env: &mut impl PaymentsEnv,
    dialog_id: DialogId,
    payment_form: telegram_api::payments_paymentFormStars,
    mut promise: Promise<td_api::PaymentForm>,
) {
    env.on_get_users(payment_form.users, "GetPaymentFormQuery 2");

    let seller_bot_user_id = UserId::new(payment_form.bot_id);
    if !seller_bot_user_id.is_valid() {
        tdutils::log_error!("Receive invalid seller {}", seller_bot_user_id.get());
        return promise_error(promise, 500, "Receive invalid seller identifier");
    }
    let invoice = payment_form
        .invoice
        .expect("payments_paymentFormStars carries its invoice");
    if invoice.prices.len() != 1 {
        tdutils::log_error!("Receive invalid prices {:?}", invoice.prices);
        return promise_error(promise, 500, "Receive invalid price");
    }
    let photo = env.web_document_photo(payment_form.photo, dialog_id);
    let star_count = get_star_count(invoice.prices[0].amount);
    let r#type = if invoice.subscription_period > 0 {
        td_api::PaymentFormType::paymentFormTypeStarSubscription(
            td_api::paymentFormTypeStarSubscription {
                pricing: Some(td_api::StarSubscriptionPricing {
                    period: invoice.subscription_period,
                    star_count,
                }),
            },
        )
    } else {
        td_api::PaymentFormType::paymentFormTypeStars(td_api::paymentFormTypeStars { star_count })
    };
    let seller_user_id = env.get_user_id_object(seller_bot_user_id, "paymentForm seller");
    let product_info =
        get_product_info_object(env, &payment_form.title, &payment_form.description, &photo);
    promise.set_value(td_api::PaymentForm {
        id: payment_form.form_id,
        r#type: Some(r#type),
        seller_bot_user_id: seller_user_id,
        product_info: Some(product_info),
    });
}

/// `ValidateRequestedInfoQuery::on_result` (Payments.cpp:626-638).
pub fn on_get_validated_order_info_result(
    validated_order_info: telegram_api::payments_ValidatedRequestedInfo,
    mut promise: Promise<td_api::ValidatedOrderInfo>,
) {
    let shipping_options: Vec<td_api::ShippingOption> = validated_order_info
        .shipping_options
        .into_iter()
        .map(|shipping_option| convert_shipping_option(Some(shipping_option)))
        .collect::<Option<Vec<_>>>()
        .expect("shipping options are never null inside the vector");
    promise.set_value(td_api::ValidatedOrderInfo {
        order_info_id: validated_order_info.id,
        shipping_options,
    });
}

/// The shared `SendPaymentFormQuery`/`SendStarPaymentFormQuery`
/// `on_result` collapse (Payments.cpp:677-703 and 730-757): the updates
/// tail answers `paymentResult(true, "")` after
/// `UpdatesManager::on_get_updates`, the verification URL answers
/// immediately.
pub fn on_get_send_payment_form_result(
    env: &mut impl PaymentsEnv,
    payment_result: telegram_api::payments_PaymentResult,
    promise: Promise<td_api::PaymentResult>,
    is_stars: bool,
) {
    let _ = is_stars; // the two C++ handlers share the collapse
    match payment_result {
        telegram_api::payments_PaymentResult::payments_paymentResult(result) => {
            let continuation = Promise::from_interface(ContinuationPromise {
                promise: Some(promise),
            });
            env.on_get_updates(result.updates.unwrap_or_default(), continuation);
        }
        telegram_api::payments_PaymentResult::payments_paymentVerificationNeeded(result) => {
            let mut promise = promise;
            promise.set_value(td_api::PaymentResult {
                success: false,
                verification_url: result.url,
            });
        }
    }
}

/// The `Promise<Unit>` continuation that completes the parked payment
/// result with `paymentResult(true, "")` (Payments.cpp:689-692).
struct ContinuationPromise {
    promise: Option<Promise<td_api::PaymentResult>>,
}

impl tdutils::promise::PromiseInterface<()> for ContinuationPromise {
    fn set_value(&mut self, _: ()) {
        if let Some(mut promise) = self.promise.take() {
            promise.set_value(td_api::PaymentResult {
                success: true,
                verification_url: String::new(),
            });
        }
    }

    fn set_error(&mut self, error: Status) {
        if let Some(mut promise) = self.promise.take() {
            promise.set_error(error);
        }
    }
}

/// `GetPaymentReceiptQuery::on_result` (Payments.cpp:786-852) for the
/// `payments_paymentReceiptStars` answer.
pub fn on_get_payment_receipt_stars_result(
    env: &mut impl PaymentsEnv,
    dialog_id: DialogId,
    payment_receipt: telegram_api::payments_paymentReceiptStars,
    mut promise: Promise<td_api::PaymentReceipt>,
) {
    env.on_get_users(payment_receipt.users, "GetPaymentReceiptQuery 1");
    let seller_bot_user_id = UserId::new(payment_receipt.bot_id);
    if !seller_bot_user_id.is_valid() {
        tdutils::log_error!("Receive invalid seller {}", seller_bot_user_id.get());
        return promise_error(promise, 500, "Receive invalid seller identifier");
    }
    let photo = env.web_document_photo(payment_receipt.photo, dialog_id);
    let invoice = payment_receipt
        .invoice
        .expect("payments_paymentReceiptStars carries its invoice");
    if invoice.prices.len() != 1 {
        tdutils::log_error!("Receive invalid prices {:?}", invoice.prices);
        return promise_error(promise, 500, "Receive invalid price");
    }
    let seller_user_id = env.get_user_id_object(seller_bot_user_id, "paymentReceipt seller");
    let product_info = get_product_info_object(
        env,
        &payment_receipt.title,
        &payment_receipt.description,
        &photo,
    );
    promise.set_value(td_api::PaymentReceipt {
        product_info: Some(product_info),
        date: payment_receipt.date,
        seller_bot_user_id: seller_user_id,
        r#type: Some(td_api::PaymentReceiptType::paymentReceiptTypeStars(
            td_api::paymentReceiptTypeStars {
                star_count: get_star_count(invoice.prices[0].amount),
                transaction_id: payment_receipt.transaction_id,
            },
        )),
    });
}

/// `GetPaymentReceiptQuery::on_result` (Payments.cpp:817-848) for the
/// `payments_paymentReceipt` answer.
pub fn on_get_payment_receipt_result(
    env: &mut impl PaymentsEnv,
    dialog_id: DialogId,
    mut payment_receipt: telegram_api::payments_paymentReceipt,
    mut promise: Promise<td_api::PaymentReceipt>,
) {
    env.on_get_users(payment_receipt.users, "GetPaymentReceiptQuery 2");
    let payments_provider_user_id = UserId::new(payment_receipt.provider_id);
    if !payments_provider_user_id.is_valid() {
        tdutils::log_error!(
            "Receive invalid payments provider {}",
            payments_provider_user_id.get()
        );
        return promise_error(promise, 500, "Receive invalid payments provider identifier");
    }
    let seller_bot_user_id = UserId::new(payment_receipt.bot_id);
    if !seller_bot_user_id.is_valid() {
        tdutils::log_error!("Receive invalid seller {}", seller_bot_user_id.get());
        return promise_error(promise, 500, "Receive invalid seller identifier");
    }
    let photo = env.web_document_photo(payment_receipt.photo, dialog_id);
    if payment_receipt.tip_amount < 0
        || !crate::misc::check_currency_amount(payment_receipt.tip_amount)
    {
        tdutils::log_error!("Receive invalid tip amount {}", payment_receipt.tip_amount);
        payment_receipt.tip_amount = 0;
    }
    let invoice = payment_receipt
        .invoice
        .expect("payments_paymentReceipt carries its invoice");
    let seller_user_id = env.get_user_id_object(seller_bot_user_id, "paymentReceipt seller");
    let provider_user_id =
        env.get_user_id_object(payments_provider_user_id, "paymentReceipt provider");
    let product_info = get_product_info_object(
        env,
        &payment_receipt.title,
        &payment_receipt.description,
        &photo,
    );
    promise.set_value(td_api::PaymentReceipt {
        product_info: Some(product_info),
        date: payment_receipt.date,
        seller_bot_user_id: seller_user_id,
        r#type: Some(td_api::PaymentReceiptType::paymentReceiptTypeRegular(
            td_api::paymentReceiptTypeRegular {
                payment_provider_user_id: provider_user_id,
                invoice: Some(convert_invoice(invoice)),
                order_info: convert_order_info_to_td(payment_receipt.info),
                shipping_option: convert_shipping_option(payment_receipt.shipping),
                credentials_title: payment_receipt.credentials_title,
                tip_amount: payment_receipt.tip_amount,
            },
        )),
    });
}

/// `GetSavedInfoQuery::on_result` (Payments.cpp:871-880).
pub fn on_get_saved_order_info_result(
    saved_info: telegram_api::payments_SavedInfo,
    mut promise: Promise<td_api::OrderInfo>,
) {
    promise.set_value(convert_order_info_to_td(saved_info.saved_info).unwrap_or_default());
}

/// `ExportInvoiceQuery::on_result` (Payments.cpp:929-937).
pub fn on_get_exported_invoice_result(
    link: telegram_api::payments_ExportedInvoice,
    mut promise: Promise<String>,
) {
    promise.set_value(link.url);
}

/// `GetBankCardInfoQuery::on_result` (Payments.cpp:957-968).
pub fn on_get_bank_card_info_result(
    response: telegram_api::payments_BankCardData,
    mut promise: Promise<td_api::BankCardInfo>,
) {
    let actions: Vec<td_api::BankCardActionOpenUrl> = response
        .open_urls
        .iter()
        .map(|open_url| td_api::BankCardActionOpenUrl {
            text: open_url.name.clone(),
            url: open_url.url.clone(),
        })
        .collect();
    promise.set_value(td_api::BankCardInfo {
        title: response.title,
        actions,
    });
}

/// `GetCollectibleInfoQuery::on_result` (Payments.cpp:988-1006).
pub fn on_get_collectible_info_result(
    mut result: telegram_api::fragment_CollectibleInfo,
    mut promise: Promise<td_api::CollectibleItemInfo>,
) {
    if result.amount <= 0 || !crate::misc::check_currency_amount(result.amount) {
        tdutils::log_error!("Receive invalid collectible item price {}", result.amount);
        result.amount = 0;
    }
    if result.crypto_currency.is_empty() || result.crypto_amount <= 0 {
        tdutils::log_error!(
            "Receive invalid collectible item cryptocurrency price {}",
            result.crypto_amount
        );
        result.crypto_amount = 0;
    }
    promise.set_value(td_api::CollectibleItemInfo {
        purchase_date: result.purchase_date,
        currency: result.currency,
        amount: result.amount,
        cryptocurrency: result.crypto_currency,
        cryptocurrency_amount: result.crypto_amount,
        url: result.url,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_manager::{ChannelId, DialogParticipantStatus};
    use std::collections::HashMap;
    use tdutils::promise::PromiseInterface;

    /// The recording-promise bridge (the M.40 RecordingPromise
    /// convention, extended to record set_error as well): both halves of
    /// a settled promise land in the slot.
    struct Recorded<T: Send + 'static>(std::sync::Arc<parking_lot::Mutex<Option<Result<T>>>>);

    struct RecordingPromise<T: Send + 'static> {
        recorded: Recorded<T>,
    }

    impl<T: Send + 'static> PromiseInterface<T> for RecordingPromise<T> {
        fn set_value(&mut self, value: T) {
            *self.recorded.0.lock() = Some(Result::Ok(value));
        }

        fn set_error(&mut self, error: Status) {
            *self.recorded.0.lock() = Some(Result::Err(error));
        }
    }

    fn recording_promise<T: Send + 'static>() -> (Promise<T>, Recorded<T>) {
        let recorded = Recorded(std::sync::Arc::new(parking_lot::Mutex::new(None)));
        (
            Promise::from_interface(RecordingPromise {
                recorded: Recorded(recorded.0.clone()),
            }),
            recorded,
        )
    }

    /// The error half of a settled flow result (the tdutils Result has no
    /// `err`/`is_ok` surface).
    fn err_of<T>(result: &Result<T>) -> Option<(i32, String)> {
        match result {
            Result::Err(status) => Some((status.code(), status.message().to_owned())),
            Result::Ok(_) => None,
        }
    }

    impl<T: Send + Clone + 'static> Recorded<T> {
        fn value(&self) -> Option<T> {
            match &*self.0.lock() {
                Some(Result::Ok(value)) => Some(value.clone()),
                Some(Result::Err(status)) => panic!("promise settled with an error: {status}"),
                None => None,
            }
        }

        fn error(&self) -> Option<(i32, String)> {
            match &*self.0.lock() {
                Some(Result::Err(status)) => Some((status.code(), status.message().to_owned())),
                Some(Result::Ok(_)) => None,
                None => None,
            }
        }

        /// The net-parked state: the seam consumed the promise without
        /// settling it (the sent-but-unanswered query shape).
        fn pending(&self) -> bool {
            self.0.lock().is_none()
        }
    }

    /// The recording env: canned user/dialog/message-plane answers, the
    /// fired net seams and the star-manager bookkeeping.
    struct TestEnv {
        unix_time: i32,
        options: HashMap<String, i64>,
        pmc: HashMap<Vec<u8>, Vec<u8>>,
        queries: Vec<String>,
        users_intake: Vec<String>,
        user_id_objects: Vec<i64>,
        have_dialogs: Vec<i64>,
        channel_admin: bool,
        star_count_balance: bool,
        star_bookkeeping: Vec<i64>,
        dismissed_actions: usize,
        photo_object: Option<td_api::Photo>,
        gift_text: Option<String>,
        invoice_message_info: Option<InvoiceMessageInfo>,
        payment_successful_message_id: Option<ServerMessageId>,
        check_business_connection_result: Option<Status>,
        invite_link_hash: String,
    }

    impl Default for TestEnv {
        fn default() -> Self {
            TestEnv {
                unix_time: 1700000000,
                options: HashMap::new(),
                pmc: HashMap::new(),
                queries: Vec::new(),
                users_intake: Vec::new(),
                user_id_objects: Vec::new(),
                have_dialogs: Vec::new(),
                channel_admin: true,
                star_count_balance: true,
                star_bookkeeping: Vec::new(),
                dismissed_actions: 0,
                photo_object: None,
                gift_text: None,
                invoice_message_info: None,
                payment_successful_message_id: None,
                check_business_connection_result: None,
                invite_link_hash: String::new(),
            }
        }
    }

    impl PaymentsEnv for TestEnv {
        fn get_unix_time(&self) -> i32 {
            self.unix_time
        }

        fn get_option_integer(&mut self, name: &str) -> i64 {
            *self.options.get(name).unwrap_or(&0)
        }

        fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
            self.pmc.get(key).cloned().unwrap_or_default()
        }

        fn get_input_user(&mut self, user_id: UserId) -> Result<telegram_api::InputUser> {
            if !user_id.is_valid() {
                return Result::from_status(Status::error(400, "User not found"));
            }
            Result::Ok(telegram_api::InputUser::inputUser(
                telegram_api::inputUser {
                    user_id: user_id.get(),
                    access_hash: 0,
                },
            ))
        }

        fn get_input_users(&mut self, user_ids: &[UserId]) -> Result<Vec<telegram_api::InputUser>> {
            let mut input_users = Vec::with_capacity(user_ids.len());
            for &user_id in user_ids {
                match self.get_input_user(user_id) {
                    Result::Ok(input_user) => input_users.push(input_user),
                    Result::Err(status) => return Result::Err(status),
                }
            }
            Result::Ok(input_users)
        }

        fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str) {
            self.users_intake.push(format!("{source}:{}", users.len()));
        }

        fn get_user_id_object(&mut self, user_id: UserId, _source: &str) -> i64 {
            self.user_id_objects.push(user_id.get());
            user_id.get()
        }

        fn have_dialog_force(&mut self, dialog_id: DialogId, _source: &str) -> bool {
            self.have_dialogs.contains(&dialog_id.get())
        }

        fn get_channel_status(&mut self, _channel_id: ChannelId) -> DialogParticipantStatus {
            if self.channel_admin {
                DialogParticipantStatus::creator(false, false, String::new())
            } else {
                DialogParticipantStatus::banned(0, String::new())
            }
        }

        fn is_broadcast_channel(&mut self, _channel_id: ChannelId) -> bool {
            true
        }

        fn get_input_peer(
            &mut self,
            dialog_id: DialogId,
            _access_rights: AccessRights,
        ) -> Option<telegram_api::InputPeer> {
            if self.have_dialogs.contains(&dialog_id.get()) {
                return Some(telegram_api::InputPeer::inputPeerEmpty(
                    telegram_api::inputPeerEmpty {},
                ));
            }
            None
        }

        fn check_dialog_access(&mut self, dialog_id: DialogId, _source: &str) -> Result<()> {
            if self.have_dialogs.contains(&dialog_id.get()) {
                Result::Ok(())
            } else {
                Result::from_status(Status::error(400, "Chat not found"))
            }
        }

        fn get_my_dialog_id(&self) -> DialogId {
            DialogId::new(777)
        }

        fn on_get_dialog_error(&mut self, dialog_id: DialogId, status: &Status, source: &str) {
            self.queries.push(format!(
                "dialog_error:{source}:{}:{}",
                dialog_id.get(),
                status.message()
            ));
        }

        fn get_invoice_message_info(
            &mut self,
            _message_full_id: MessageFullId,
        ) -> Result<InvoiceMessageInfo> {
            match self.invoice_message_info {
                Some(info) => Result::Ok(info),
                None => Result::from_status(Status::error(400, "Message not found")),
            }
        }

        fn get_payment_successful_message_id(
            &mut self,
            _message_full_id: MessageFullId,
        ) -> Result<ServerMessageId> {
            match self.payment_successful_message_id {
                Some(id) => Result::Ok(id),
                None => Result::from_status(Status::error(400, "Message has wrong type")),
            }
        }

        fn has_owned_star_count(&mut self, _star_count: i64) -> bool {
            self.star_count_balance
        }

        fn add_pending_owned_star_count(&mut self, star_count: i64, _only_first: bool) {
            self.star_bookkeeping.push(star_count);
        }

        fn dismiss_suggested_action(&mut self) {
            self.dismissed_actions += 1;
        }

        fn check_business_connection(
            &mut self,
            _business_connection_id: &BusinessConnectionId,
            _dialog_id: DialogId,
        ) -> std::result::Result<(), Status> {
            match self.check_business_connection_result.take() {
                Some(status) => std::result::Result::Err(status),
                None => std::result::Result::Ok(()),
            }
        }

        fn photo_object(&mut self, _photo: &Photo) -> Option<td_api::Photo> {
            self.photo_object.clone()
        }

        fn premium_gift_text(
            &mut self,
            text: Option<td_api::FormattedText>,
        ) -> Result<Option<telegram_api::TextWithEntities>> {
            let Some(text) = text else {
                return Result::Ok(None);
            };
            if text.text.is_empty() {
                return Result::Ok(None);
            }
            Result::Ok(
                self.gift_text
                    .clone()
                    .map(|gift| telegram_api::TextWithEntities {
                        text: gift,
                        entities: Vec::new(),
                    }),
            )
        }

        fn get_dialog_invite_link_hash(&mut self, _invite_link: &str) -> String {
            self.invite_link_hash.clone()
        }

        fn send_set_bot_shipping_answer_query(
            &mut self,
            shipping_query_id: i64,
            error_message: &str,
            shipping_options: Vec<telegram_api::ShippingOption>,
            _promise: Promise<()>,
        ) {
            self.queries.push(format!(
                "setBotShippingResults:{shipping_query_id}:{error_message}:{}",
                shipping_options.len()
            ));
        }

        fn send_set_bot_pre_checkout_answer_query(
            &mut self,
            pre_checkout_query_id: i64,
            error_message: &str,
            _promise: Promise<()>,
        ) {
            self.queries.push(format!(
                "setBotPrecheckoutResults:{pre_checkout_query_id}:{error_message}"
            ));
        }

        fn send_get_payment_form_query(
            &mut self,
            _input_invoice: telegram_api::InputInvoice,
            theme_parameters: Option<telegram_api::DataJSON>,
            _promise: Promise<td_api::PaymentForm>,
        ) {
            self.queries
                .push(format!("getPaymentForm:{}", theme_parameters.is_some()));
        }

        fn send_validate_requested_info_query(
            &mut self,
            _input_invoice: telegram_api::InputInvoice,
            requested_info: Option<telegram_api::PaymentRequestedInfo>,
            allow_save: bool,
            _promise: Promise<td_api::ValidatedOrderInfo>,
        ) {
            self.queries.push(format!(
                "validateRequestedInfo:{}:{}",
                allow_save,
                requested_info.is_some()
            ));
        }

        fn send_send_payment_form_query(
            &mut self,
            _input_invoice: telegram_api::InputInvoice,
            payment_form_id: i64,
            order_info_id: &str,
            shipping_option_id: &str,
            _input_credentials: telegram_api::InputPaymentCredentials,
            tip_amount: i64,
            _promise: Promise<td_api::PaymentResult>,
        ) {
            self.queries.push(format!(
                "sendPaymentForm:{payment_form_id}:{order_info_id}:{shipping_option_id}:{tip_amount}"
            ));
        }

        fn send_send_stars_form_query(
            &mut self,
            payment_form_id: i64,
            _input_invoice: telegram_api::InputInvoice,
            _promise: Promise<td_api::PaymentResult>,
        ) {
            self.queries
                .push(format!("sendStarsForm:{payment_form_id}"));
        }

        fn send_get_payment_receipt_query(
            &mut self,
            dialog_id: DialogId,
            server_message_id: ServerMessageId,
            _promise: Promise<td_api::PaymentReceipt>,
        ) {
            self.queries.push(format!(
                "getPaymentReceipt:{}:{}",
                dialog_id.get(),
                server_message_id.get()
            ));
        }

        fn send_get_saved_info_query(&mut self, _promise: Promise<td_api::OrderInfo>) {
            self.queries.push("getSavedInfo".to_owned());
        }

        fn send_clear_saved_info_query(
            &mut self,
            clear_credentials: bool,
            clear_order_info: bool,
            _promise: Promise<()>,
        ) {
            self.queries.push(format!(
                "clearSavedInfo:{clear_credentials}:{clear_order_info}"
            ));
        }

        fn send_export_invoice_query(
            &mut self,
            business_connection_id: &BusinessConnectionId,
            _input_media_invoice: telegram_api::inputMediaInvoice,
            _promise: Promise<String>,
        ) {
            self.queries
                .push(format!("exportInvoice:{}", business_connection_id.get()));
        }

        fn send_get_bank_card_info_query(
            &mut self,
            bank_card_number: &str,
            _promise: Promise<td_api::BankCardInfo>,
        ) {
            self.queries
                .push(format!("getBankCardData:{bank_card_number}"));
        }

        fn send_get_collectible_info_query(
            &mut self,
            input_collectible: telegram_api::InputCollectible,
            _promise: Promise<td_api::CollectibleItemInfo>,
        ) {
            let name = match input_collectible {
                telegram_api::InputCollectible::inputCollectibleUsername(v) => v.username,
                telegram_api::InputCollectible::inputCollectiblePhone(v) => v.phone,
            };
            self.queries.push(format!("getCollectibleInfo:{name}"));
        }
    }

    // --- the value-plane conversion tests ---

    fn wire_invoice() -> telegram_api::Invoice {
        telegram_api::Invoice {
            flags: 0,
            test: true,
            name_requested: false,
            phone_requested: true,
            email_requested: false,
            shipping_address_requested: false,
            flexible: false,
            phone_to_provider: true,
            email_to_provider: false,
            recurring: false,
            currency: "USD".to_owned(),
            prices: vec![
                telegram_api::LabeledPrice {
                    label: "base".to_owned(),
                    amount: 100,
                },
                telegram_api::LabeledPrice {
                    label: "tax".to_owned(),
                    amount: 10,
                },
            ],
            max_tip_amount: 5,
            suggested_tip_amounts: vec![1, 2, -3, 4, 5, 1 << 41],
            terms_url: "https://example.com/terms".to_owned(),
            subscription_period: 86400,
        }
    }

    #[test]
    fn convert_invoice_derives_flags_and_clamps() {
        let invoice = convert_invoice(wire_invoice());
        assert_eq!(invoice.currency, "USD");
        assert!(invoice.is_test);
        assert!(invoice.need_phone_number);
        assert!(invoice.send_phone_number_to_provider);
        // the tip clamp: the negative entry and the over-limit sentinel go
        // away, and the vector truncates to 4
        assert_eq!(invoice.suggested_tip_amounts, vec![1, 2, 4, 5]);
        assert_eq!(invoice.max_tip_amount, 5);
        assert_eq!(invoice.subscription_period, 86400);
        assert_eq!(invoice.price_parts.len(), 2);
        assert_eq!(invoice.price_parts[0].amount, 100);

        // the flexible flag forces the shipping address; the recurring
        // terms URL split
        let mut wire = wire_invoice();
        wire.flexible = true;
        wire.recurring = true;
        let invoice = convert_invoice(wire);
        assert!(invoice.is_flexible);
        assert!(invoice.need_shipping_address);
        assert_eq!(
            invoice.recurring_payment_terms_of_service_url,
            "https://example.com/terms"
        );
        assert_eq!(invoice.terms_of_service_url, "");

        // the invalid labeled price amount clamps to the ±2^40 sentinel
        let mut wire = wire_invoice();
        wire.prices[0].amount = -(1i64 << 41);
        let invoice = convert_invoice(wire);
        assert_eq!(invoice.price_parts[0].amount, -(1i64 << 40));
    }

    #[test]
    fn convert_payment_provider_smartglocal_and_stripe() {
        let smartglocal = convert_payment_provider(
            "smartglocal",
            Some(telegram_api::DataJSON {
                data: r#"{"public_token":"tok","tokenize_url":"https://pay.smart-glocal.com/cds/v1/tokenize/card"}"#
                    .to_owned(),
            }),
            true,
        );
        let Some(td_api::PaymentProvider::paymentProviderSmartGlocal(v)) = smartglocal else {
            panic!("wrong provider kind");
        };
        assert_eq!(v.public_token, "tok");
        assert_eq!(
            v.tokenize_url,
            "https://pay.smart-glocal.com/cds/v1/tokenize/card"
        );

        // the override URL must keep the smart-glocal tokenize shape
        let rejected = convert_payment_provider(
            "smartglocal",
            Some(telegram_api::DataJSON {
                data: r#"{"public_token":"tok","tokenize_url":"https://evil.example.com/cds/v1/tokenize/card"}"#
                    .to_owned(),
            }),
            false,
        );
        let Some(td_api::PaymentProvider::paymentProviderSmartGlocal(v)) = rejected else {
            panic!("wrong provider kind");
        };
        assert_eq!(
            v.tokenize_url,
            "https://tgb.smart-glocal.com/cds/v1/tokenize/card"
        );

        // unknown providers and broken JSON answer None (the C++ null)
        assert!(
            convert_payment_provider(
                "other",
                Some(telegram_api::DataJSON {
                    data: "{}".to_owned()
                }),
                false
            )
            .is_none()
        );
        assert!(
            convert_payment_provider(
                "smartglocal",
                Some(telegram_api::DataJSON {
                    data: "not json".to_owned()
                }),
                false
            )
            .is_none()
        );
        assert!(convert_payment_provider("smartglocal", None, false).is_none());

        let stripe = convert_payment_provider(
            "stripe",
            Some(telegram_api::DataJSON {
                data: r#"{"need_country":true,"need_zip":false,"need_cardholder_name":true,"publishable_key":"pk"}"#
                    .to_owned(),
            }),
            false,
        );
        let Some(td_api::PaymentProvider::paymentProviderStripe(v)) = stripe else {
            panic!("wrong provider kind");
        };
        assert!(v.need_country);
        assert!(!v.need_postal_code);
        assert!(v.need_cardholder_name);
        assert_eq!(v.publishable_key, "pk");

        // the stripe branch requires every field
        assert!(
            convert_payment_provider(
                "stripe",
                Some(telegram_api::DataJSON {
                    data: r#"{"need_country":true}"#.to_owned()
                }),
                false,
            )
            .is_none()
        );
    }

    #[test]
    fn convert_order_info_and_address_round_trip() {
        let td_address = td_api::Address {
            country_code: "DE".to_owned(),
            state: "BY".to_owned(),
            city: "Munich".to_owned(),
            street_line1: "Marienplatz 1".to_owned(),
            street_line2: String::new(),
            postal_code: "80331".to_owned(),
        };
        let wire = convert_address_to_telegram(Some(td_address.clone())).unwrap();
        assert_eq!(wire.country_iso2, "DE");
        assert_eq!(wire.post_code, "80331");
        let back = convert_address_to_td(Some(wire)).unwrap();
        assert_eq!(back, td_address);
        assert!(convert_address_to_td(None).is_none());
        assert!(convert_address_to_telegram(None).is_none());

        let td_order = td_api::OrderInfo {
            name: "Alice".to_owned(),
            phone_number: "+123".to_owned(),
            email_address: String::new(),
            shipping_address: Some(td_address),
        };
        let wire = convert_order_info_to_telegram(Some(td_order.clone())).unwrap();
        // the flags derive from the non-empty fields (email empty)
        assert_eq!(wire.flags, 1 | 2 | 8);
        let back = convert_order_info_to_td(Some(wire)).unwrap();
        assert_eq!(back, td_order);
        assert!(convert_order_info_to_td(None).is_none());
    }

    #[test]
    fn convert_shipping_option_and_saved_credentials() {
        let wire = telegram_api::ShippingOption {
            id: "opt1".to_owned(),
            title: "Standard".to_owned(),
            prices: vec![telegram_api::LabeledPrice {
                label: "fee".to_owned(),
                amount: 42,
            }],
        };
        let option = convert_shipping_option(Some(wire)).unwrap();
        assert_eq!(option.id, "opt1");
        assert_eq!(option.price_parts[0].label, "fee");
        assert!(convert_shipping_option(None).is_none());

        let credentials = convert_saved_credentials(vec![telegram_api::PaymentSavedCredentials {
            id: "cid".to_owned(),
            title: "Visa".to_owned(),
        }]);
        assert_eq!(credentials.len(), 1);
        assert_eq!(credentials[0].id, "cid");
    }

    #[test]
    fn labeled_price_parts_validate_and_sum() {
        let parts = |items: &[(&str, i64)]| {
            items
                .iter()
                .map(|(label, amount)| td_api::LabeledPricePart {
                    label: (*label).to_owned(),
                    amount: *amount,
                })
                .collect::<Vec<_>>()
        };
        let mut total = 0i64;
        let converted = LabeledPricePart::get_labeled_price_parts(
            parts(&[("a", 10), ("b", 32)]),
            Some(&mut total),
        )
        .unwrap();
        assert_eq!(total, 42);
        assert_eq!(converted.len(), 2);

        // the validation ladder (LabeledPricePart.cpp:24-55)
        assert_eq!(
            err_of(&LabeledPricePart::get_labeled_price_parts(Vec::new(), None)),
            Some((400, "There must be at least one price part".to_owned()))
        );
        assert_eq!(
            err_of(&LabeledPricePart::get_labeled_price_parts(
                parts(&[("a", 0)]),
                None
            )),
            Some((400, "Total price must be positive".to_owned()))
        );
        assert_eq!(
            err_of(&LabeledPricePart::get_labeled_price_parts(
                parts(&[("a", 1 << 41)]),
                None
            )),
            Some((400, "Too big amount of the currency specified".to_owned()))
        );
        assert_eq!(
            err_of(&LabeledPricePart::get_labeled_price_parts(
                parts(&[("a", 9999_9999_9999), ("b", 1)]),
                None
            )),
            Some((400, "Total price is too big".to_owned()))
        );

        let input = LabeledPricePart::get_input_labeled_prices(&converted);
        assert_eq!(input.len(), 2);
        assert_eq!(input[0].label, "a");
        assert_eq!(input[1].amount, 32);
    }

    #[test]
    fn star_count_clamps_like_the_cpp() {
        assert_eq!(get_star_count(100), 100);
        assert_eq!(get_star_count(0), 0);
        assert_eq!(get_star_count(-5), 0, "negative counts log and answer 0");
        assert_eq!(get_star_count((1i64 << 51) + 1), 1i64 << 51);
    }

    #[test]
    fn check_payment_amount_validates_currency_and_amount() {
        let mut currency = "USD".to_owned();
        assert!(err_of(&check_payment_amount(&mut currency, 100)).is_none());
        assert_eq!(currency, "USD");
        assert_eq!(
            err_of(&check_payment_amount(&mut currency, 0)),
            Some((400, "Invalid amount of the currency specified".to_owned()))
        );
        assert_eq!(
            err_of(&check_payment_amount(&mut currency, -(1 << 41))),
            Some((400, "Invalid amount of the currency specified".to_owned()))
        );
    }

    #[test]
    fn temp_password_state_parses_the_pmc_row_and_expires() {
        let mut env = TestEnv::default();

        // no row → no password
        assert!(!get_temp_password_state_sync(&mut env).has_temp_password);

        // the store format is string + int32 (TempPasswordState.hpp)
        let mut row = Vec::new();
        crate::store_parse::store_string(&mut row, "pw123");
        crate::store_parse::store_i32(&mut row, 1700000100);
        env.pmc.insert(b"temp_password".to_vec(), row);
        let state = get_temp_password_state_sync(&mut env);
        assert!(state.has_temp_password);
        assert_eq!(state.temp_password, "pw123");

        // expired at unix_time
        env.unix_time = 1700000100;
        assert!(!get_temp_password_state_sync(&mut env).has_temp_password);
    }

    #[test]
    fn theme_parameters_json_keeps_order_and_colors() {
        let json = get_theme_parameters_json_string(&td_api::ThemeParameters {
            background_color: 0x00FFFF,
            text_color: 0x102030,
            ..Default::default()
        });
        assert!(json.starts_with(r##"{"bg_color":"#00ffff","secondary_bg_color":"#000000""##));
        assert!(json.contains(r##""text_color":"#102030""##));
        assert!(json.ends_with(r##""destructive_text_color":"#000000"}"##));
        assert_eq!(json.matches(r##"",""##).count(), 14, "15 ordered fields");
    }

    // --- the flow tests ---

    #[test]
    fn answer_shipping_query_validates_and_sends() {
        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        answer_shipping_query(
            &mut env,
            42,
            vec![td_api::ShippingOption {
                id: "opt".to_owned(),
                title: "Post".to_owned(),
                price_parts: vec![td_api::LabeledPricePart {
                    label: "fee".to_owned(),
                    amount: 7,
                }],
            }],
            "",
            promise,
        );
        assert_eq!(env.queries, vec!["setBotShippingResults:42::1"]);
        assert!(recorded.pending(), "the net seam parks the promise");

        // the empty-option error precedes the send
        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        answer_shipping_query(&mut env, 1, Vec::new(), "", promise);
        assert!(env.queries.is_empty());
        assert_eq!(
            recorded.error(),
            Some((400, "There must be at least one shipping option".to_owned()))
        );

        // the price-part validation ladder answers through the same promise
        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        answer_shipping_query(
            &mut env,
            1,
            vec![td_api::ShippingOption {
                id: "opt".to_owned(),
                title: "Post".to_owned(),
                price_parts: Vec::new(),
            }],
            "",
            promise,
        );
        assert_eq!(
            recorded.error(),
            Some((400, "There must be at least one price part".to_owned()))
        );
    }

    #[test]
    fn answer_pre_checkout_query_parks_the_seam() {
        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        answer_pre_checkout_query(&mut env, 7, "out of stock", promise);
        assert_eq!(env.queries, vec!["setBotPrecheckoutResults:7:out of stock"]);
        assert!(recorded.pending(), "the net seam parks the promise");
    }

    fn name_invoice() -> td_api::InputInvoice {
        td_api::InputInvoice::inputInvoiceName(td_api::inputInvoiceName {
            name: "test-invoice".to_owned(),
        })
    }

    #[test]
    fn get_payment_form_requires_an_invoice_and_builds_the_theme() {
        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        get_payment_form(&mut env, None, None, promise);
        assert_eq!(
            recorded.error(),
            Some((400, "Input invoice must be non-empty".to_owned()))
        );

        let mut env = TestEnv::default();
        let (promise, _recorded) = recording_promise();
        get_payment_form(
            &mut env,
            Some(name_invoice()),
            Some(&td_api::ThemeParameters {
                background_color: 0x112233,
                ..Default::default()
            }),
            promise,
        );
        assert_eq!(env.queries, vec!["getPaymentForm:true"]);

        // no theme → the theme_params half stays unset
        let mut env = TestEnv::default();
        let (promise, _recorded) = recording_promise();
        get_payment_form(&mut env, Some(name_invoice()), None, promise);
        assert_eq!(env.queries, vec!["getPaymentForm:false"]);
    }

    #[test]
    fn get_payment_form_invoice_message_needs_the_message_and_the_peer() {
        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        get_payment_form(
            &mut env,
            Some(td_api::InputInvoice::inputInvoiceMessage(
                td_api::inputInvoiceMessage {
                    chat_id: -1000000000001,
                    message_id: 1,
                },
            )),
            None,
            promise,
        );
        // the MessagesManager seam is [blocked: M.10] and errors
        assert_eq!(
            recorded.error(),
            Some((400, "Message not found".to_owned()))
        );

        // with the message-info seam answered, the peer gate applies
        let mut env = TestEnv {
            invoice_message_info: Some(InvoiceMessageInfo {
                server_message_id: ServerMessageId::new(5),
                star_count: 0,
            }),
            ..TestEnv::default()
        };
        let (promise, recorded) = recording_promise();
        get_payment_form(
            &mut env,
            Some(td_api::InputInvoice::inputInvoiceMessage(
                td_api::inputInvoiceMessage {
                    chat_id: -1000000000001,
                    message_id: 1,
                },
            )),
            None,
            promise,
        );
        assert_eq!(
            recorded.error(),
            Some((400, "Can't access the chat".to_owned()))
        );
    }

    #[test]
    fn validate_order_info_checks_utf8_and_sends() {
        let mut env = TestEnv::default();
        let (promise, _recorded) = recording_promise();
        validate_order_info(&mut env, Some(name_invoice()), None, true, promise);
        assert_eq!(env.queries, vec!["validateRequestedInfo:true:true"]);

        // CLEAN_INPUT_STRING replaces the control characters (the C++
        // rejection is invalid UTF-8, which a typed Rust string cannot
        // carry); the flow proceeds with the cleaned form
        let order = td_api::OrderInfo {
            name: "a\u{1}b".to_owned(),
            ..Default::default()
        };
        let mut env = TestEnv::default();
        let (promise, _recorded) = recording_promise();
        validate_order_info(&mut env, Some(name_invoice()), Some(order), false, promise);
        assert_eq!(env.queries, vec!["validateRequestedInfo:false:true"]);

        // the shipping-address fields validate one by one
        let order = td_api::OrderInfo {
            shipping_address: Some(td_api::Address {
                postal_code: "80\u{1}331".to_owned(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut env = TestEnv::default();
        let (promise, _recorded) = recording_promise();
        validate_order_info(&mut env, Some(name_invoice()), Some(order), false, promise);
        assert_eq!(env.queries, vec!["validateRequestedInfo:false:true"]);
    }

    #[test]
    fn send_payment_form_stars_branch_books_the_pending_count() {
        // credentials == nullptr + a non-empty order_info_id → the
        // invalid-parameters error
        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        send_payment_form(
            &mut env,
            Some(name_invoice()),
            99,
            "order",
            "shipping",
            None,
            5,
            promise,
        );
        assert!(env.queries.is_empty());
        assert!(env.star_bookkeeping.is_empty());
        assert_eq!(
            recorded.error(),
            Some((400, "Invalid payment form parameters specified".to_owned()))
        );

        let mut env = TestEnv::default();
        let (promise, _recorded) = recording_promise();
        send_payment_form(&mut env, Some(name_invoice()), 99, "", "", None, 0, promise);
        assert_eq!(env.queries, vec!["sendStarsForm:99"]);
        assert_eq!(env.star_bookkeeping, vec![0], "star_count 0 parks -0");

        // an empty balance answers the stars error
        let mut env = TestEnv {
            star_count_balance: false,
            ..TestEnv::default()
        };
        let (promise, recorded) = recording_promise();
        send_payment_form(&mut env, Some(name_invoice()), 99, "", "", None, 0, promise);
        assert!(env.queries.is_empty());
        assert_eq!(
            recorded.error(),
            Some((
                400,
                "Have not enough Telegram Stars to complete payment".to_owned()
            ))
        );
    }

    #[test]
    fn send_payment_form_saved_credentials_require_the_temp_password() {
        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        send_payment_form(
            &mut env,
            Some(name_invoice()),
            99,
            "",
            "",
            Some(td_api::InputCredentials::inputCredentialsSaved(
                td_api::inputCredentialsSaved {
                    saved_credentials_id: "cid".to_owned(),
                },
            )),
            0,
            promise,
        );
        assert_eq!(
            recorded.error(),
            Some((
                400,
                "Temporary password required to use saved credentials".to_owned()
            ))
        );

        // with the pmc row present the query goes out
        let mut env = TestEnv::default();
        let mut row = Vec::new();
        crate::store_parse::store_string(&mut row, "pw");
        crate::store_parse::store_i32(&mut row, env.unix_time + 100);
        env.pmc.insert(b"temp_password".to_vec(), row);
        let (promise, _recorded) = recording_promise();
        send_payment_form(
            &mut env,
            Some(name_invoice()),
            99,
            "",
            "",
            Some(td_api::InputCredentials::inputCredentialsSaved(
                td_api::inputCredentialsSaved {
                    saved_credentials_id: "cid".to_owned(),
                },
            )),
            0,
            promise,
        );
        assert_eq!(env.queries, vec!["sendPaymentForm:99:::0"]);
    }

    #[test]
    fn send_payment_form_new_google_pay_apple_pay_and_the_tip_triple() {
        for credentials in [
            td_api::InputCredentials::inputCredentialsNew(td_api::inputCredentialsNew {
                data: "{}".to_owned(),
                allow_save: true,
            }),
            td_api::InputCredentials::inputCredentialsGooglePay(
                td_api::inputCredentialsGooglePay {
                    data: "{}".to_owned(),
                },
            ),
            td_api::InputCredentials::inputCredentialsApplePay(td_api::inputCredentialsApplePay {
                data: "{}".to_owned(),
            }),
        ] {
            let mut env = TestEnv::default();
            let (promise, _recorded) = recording_promise();
            send_payment_form(
                &mut env,
                Some(name_invoice()),
                99,
                "",
                "",
                Some(credentials),
                0,
                promise,
            );
            assert_eq!(env.queries, vec!["sendPaymentForm:99:::0"]);
        }

        // the tip/order/shipping triple rides the send args
        let mut env = TestEnv::default();
        let (promise, _recorded) = recording_promise();
        send_payment_form(
            &mut env,
            Some(name_invoice()),
            99,
            "oid",
            "sid",
            Some(td_api::InputCredentials::inputCredentialsNew(
                td_api::inputCredentialsNew {
                    data: String::new(),
                    allow_save: false,
                },
            )),
            25,
            promise,
        );
        assert_eq!(env.queries, vec!["sendPaymentForm:99:oid:sid:25"]);
    }

    #[test]
    fn get_payment_receipt_gates_on_the_message_seam() {
        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        get_payment_receipt(
            &mut env,
            MessageFullId::new(DialogId::new(100), MessageId::new(5)),
            promise,
        );
        assert_eq!(
            recorded.error(),
            Some((400, "Message has wrong type".to_owned()))
        );

        let mut env = TestEnv {
            payment_successful_message_id: Some(ServerMessageId::new(500)),
            ..TestEnv::default()
        };
        let (promise, _recorded) = recording_promise();
        get_payment_receipt(
            &mut env,
            MessageFullId::new(DialogId::new(100), MessageId::new(5)),
            promise,
        );
        assert_eq!(env.queries, vec!["getPaymentReceipt:100:500"]);
    }

    #[test]
    fn saved_info_flips_ride_the_clear_flags() {
        let mut env = TestEnv::default();
        let (promise, _recorded) = recording_promise();
        get_saved_order_info(&mut env, promise);
        assert_eq!(env.queries, vec!["getSavedInfo"]);

        let mut env = TestEnv::default();
        let (promise, _recorded) = recording_promise();
        delete_saved_order_info(&mut env, promise);
        assert_eq!(env.queries, vec!["clearSavedInfo:false:true"]);

        let mut env = TestEnv::default();
        let (promise, _recorded) = recording_promise();
        delete_saved_credentials(&mut env, promise);
        assert_eq!(env.queries, vec!["clearSavedInfo:true:false"]);
    }

    fn export_invoice_message(invoice: td_api::Invoice) -> td_api::InputMessageContent {
        td_api::InputMessageContent::inputMessageInvoice(td_api::inputMessageInvoice {
            invoice: Some(invoice),
            title: "T".to_owned(),
            description: "D".to_owned(),
            ..Default::default()
        })
    }

    fn valid_export_invoice() -> td_api::Invoice {
        td_api::Invoice {
            currency: "USD".to_owned(),
            price_parts: vec![td_api::LabeledPricePart {
                label: "base".to_owned(),
                amount: 100,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn export_invoice_validates_and_gates_the_business_connection() {
        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        export_invoice(
            &mut env,
            BusinessConnectionId::default(),
            td_api::InputMessageContent::inputMessageInvoice(td_api::inputMessageInvoice {
                invoice: None,
                ..Default::default()
            }),
            promise,
        );
        assert_eq!(
            recorded.error(),
            Some((400, "Invoice must be non-empty".to_owned()))
        );

        // a valid invoice goes through the price validation, the
        // business-connection check and the send seam
        let mut env = TestEnv::default();
        let (promise, _recorded) = recording_promise();
        export_invoice(
            &mut env,
            BusinessConnectionId::new("conn".to_owned()),
            export_invoice_message(valid_export_invoice()),
            promise,
        );
        assert_eq!(env.queries, vec!["exportInvoice:conn"]);

        // the connection check failure propagates
        let mut env = TestEnv {
            check_business_connection_result: Some(Status::error(
                400,
                "BUSINESS_CONNECTION_INVALID",
            )),
            ..TestEnv::default()
        };
        let (promise, recorded) = recording_promise();
        export_invoice(
            &mut env,
            BusinessConnectionId::new("conn".to_owned()),
            export_invoice_message(valid_export_invoice()),
            promise,
        );
        assert_eq!(
            recorded.error(),
            Some((400, "BUSINESS_CONNECTION_INVALID".to_owned()))
        );
    }

    #[test]
    fn export_invoice_price_parts_and_tips_validate() {
        let message_with = |invoice: td_api::Invoice| {
            export_invoice_message(td_api::Invoice {
                currency: "USD".to_owned(),
                price_parts: vec![td_api::LabeledPricePart {
                    label: "base".to_owned(),
                    amount: 100,
                }],
                ..invoice
            })
        };

        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        export_invoice(
            &mut env,
            BusinessConnectionId::default(),
            message_with(td_api::Invoice {
                max_tip_amount: -1,
                ..Default::default()
            }),
            promise,
        );
        assert_eq!(
            recorded.error(),
            Some((
                400,
                "Invalid max_tip_amount of the currency specified".to_owned()
            ))
        );

        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        export_invoice(
            &mut env,
            BusinessConnectionId::default(),
            message_with(td_api::Invoice {
                max_tip_amount: 10,
                suggested_tip_amounts: vec![20],
                ..Default::default()
            }),
            promise,
        );
        assert_eq!(
            recorded.error(),
            Some((
                400,
                "Suggested tip amount can't be bigger than max_tip_amount".to_owned()
            ))
        );

        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        export_invoice(
            &mut env,
            BusinessConnectionId::default(),
            message_with(td_api::Invoice {
                max_tip_amount: 10,
                suggested_tip_amounts: vec![1, 2, 3, 4, 5],
                ..Default::default()
            }),
            promise,
        );
        assert_eq!(
            recorded.error(),
            Some((
                400,
                "There can be at most 4 suggested tip amounts".to_owned()
            ))
        );

        // the happy path reaches the seam
        let mut env = TestEnv::default();
        let (promise, _recorded) = recording_promise();
        export_invoice(
            &mut env,
            BusinessConnectionId::default(),
            message_with(td_api::Invoice {
                max_tip_amount: 10,
                suggested_tip_amounts: vec![1, 2],
                ..Default::default()
            }),
            promise,
        );
        assert_eq!(env.queries, vec!["exportInvoice:"]);
    }

    #[test]
    fn get_bank_card_info_and_collectible_info_park_their_seams() {
        let mut env = TestEnv::default();
        let (promise, _recorded) = recording_promise();
        get_bank_card_info(&mut env, "4242 4242 4242 4242", promise);
        assert_eq!(env.queries, vec!["getBankCardData:4242 4242 4242 4242"]);

        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        get_collectible_info(&mut env, None, promise);
        assert_eq!(
            recorded.error(),
            Some((400, "Item type must be non-empty".to_owned()))
        );

        let mut env = TestEnv::default();
        let (promise, _recorded) = recording_promise();
        get_collectible_info(
            &mut env,
            Some(td_api::CollectibleItemType::collectibleItemTypeUsername(
                td_api::collectibleItemTypeUsername {
                    username: "durov".to_owned(),
                },
            )),
            promise,
        );
        assert_eq!(env.queries, vec!["getCollectibleInfo:durov"]);

        let mut env = TestEnv::default();
        let (promise, _recorded) = recording_promise();
        get_collectible_info(
            &mut env,
            Some(td_api::CollectibleItemType::collectibleItemTypePhoneNumber(
                td_api::collectibleItemTypePhoneNumber {
                    phone_number: "888".to_owned(),
                },
            )),
            promise,
        );
        assert_eq!(env.queries, vec!["getCollectibleInfo:888"]);
    }

    // --- the input-invoice telegram-purpose tests ---

    #[test]
    fn input_invoice_telegram_purpose_stars_dismisses_and_sends() {
        let mut env = TestEnv::default();
        env.have_dialogs.push(1000);
        let (promise, _recorded) = recording_promise();
        get_payment_form(
            &mut env,
            Some(td_api::InputInvoice::inputInvoiceTelegram(
                td_api::inputInvoiceTelegram {
                    purpose: Some(td_api::TelegramPaymentPurpose::telegramPaymentPurposeStars(
                        td_api::telegramPaymentPurposeStars {
                            currency: "USD".to_owned(),
                            amount: 500,
                            star_count: 100,
                            chat_id: 1000,
                        },
                    )),
                },
            )),
            None,
            promise,
        );
        assert_eq!(env.dismissed_actions, 1);
        assert_eq!(env.queries, vec!["getPaymentForm:false"]);

        // the purpose gate fails for an inaccessible spend dialog
        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        get_payment_form(
            &mut env,
            Some(td_api::InputInvoice::inputInvoiceTelegram(
                td_api::inputInvoiceTelegram {
                    purpose: Some(td_api::TelegramPaymentPurpose::telegramPaymentPurposeStars(
                        td_api::telegramPaymentPurposeStars {
                            currency: "USD".to_owned(),
                            amount: 500,
                            star_count: 100,
                            chat_id: 404,
                        },
                    )),
                },
            )),
            None,
            promise,
        );
        assert_eq!(env.dismissed_actions, 0);
        assert_eq!(recorded.error(), Some((400, "Chat not found".to_owned())));

        // no spend dialog → no access check, the dismiss still fires
        let mut env = TestEnv::default();
        let (promise, _recorded) = recording_promise();
        get_payment_form(
            &mut env,
            Some(td_api::InputInvoice::inputInvoiceTelegram(
                td_api::inputInvoiceTelegram {
                    purpose: Some(td_api::TelegramPaymentPurpose::telegramPaymentPurposeStars(
                        td_api::telegramPaymentPurposeStars {
                            currency: "USD".to_owned(),
                            amount: 500,
                            star_count: 100,
                            chat_id: 0,
                        },
                    )),
                },
            )),
            None,
            promise,
        );
        assert_eq!(env.dismissed_actions, 1);
    }

    #[test]
    fn input_invoice_telegram_purpose_join_chat_needs_the_hash() {
        let invite = |invite_link: &str| {
            td_api::InputInvoice::inputInvoiceTelegram(td_api::inputInvoiceTelegram {
                purpose: Some(
                    td_api::TelegramPaymentPurpose::telegramPaymentPurposeJoinChat(
                        td_api::telegramPaymentPurposeJoinChat {
                            invite_link: invite_link.to_owned(),
                        },
                    ),
                ),
            })
        };

        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        get_payment_form(&mut env, Some(invite("https://t.me/+abc")), None, promise);
        assert_eq!(
            recorded.error(),
            Some((400, "Invalid invite link".to_owned()))
        );

        let mut env = TestEnv {
            invite_link_hash: "abc".to_owned(),
            ..TestEnv::default()
        };
        let (promise, _recorded) = recording_promise();
        get_payment_form(&mut env, Some(invite("https://t.me/+abc")), None, promise);
        assert_eq!(env.queries, vec!["getPaymentForm:false"]);
    }

    #[test]
    fn input_invoice_telegram_purpose_premium_gift_builds_both_wire_forms() {
        // the XTR currency builds the inputInvoicePremiumGiftStars shape
        let mut env = TestEnv::default();
        let (promise, _recorded) = recording_promise();
        get_payment_form(
            &mut env,
            Some(td_api::InputInvoice::inputInvoiceTelegram(
                td_api::inputInvoiceTelegram {
                    purpose: Some(
                        td_api::TelegramPaymentPurpose::telegramPaymentPurposePremiumGift(
                            td_api::telegramPaymentPurposePremiumGift {
                                currency: "XTR".to_owned(),
                                amount: 250,
                                user_id: 1,
                                month_count: 3,
                                text: None,
                            },
                        ),
                    ),
                },
            )),
            None,
            promise,
        );
        assert_eq!(env.queries, vec!["getPaymentForm:false"]);

        // the fiat currency builds the gift-code shape
        let mut env = TestEnv::default();
        let (promise, _recorded) = recording_promise();
        get_payment_form(
            &mut env,
            Some(td_api::InputInvoice::inputInvoiceTelegram(
                td_api::inputInvoiceTelegram {
                    purpose: Some(
                        td_api::TelegramPaymentPurpose::telegramPaymentPurposePremiumGift(
                            td_api::telegramPaymentPurposePremiumGift {
                                currency: "USD".to_owned(),
                                amount: 999,
                                user_id: 1,
                                month_count: 12,
                                text: Some(td_api::FormattedText {
                                    text: "enjoy".to_owned(),
                                    entities: Vec::new(),
                                }),
                            },
                        ),
                    ),
                },
            )),
            None,
            promise,
        );
        assert_eq!(env.queries, vec!["getPaymentForm:false"]);
    }

    #[test]
    fn input_invoice_purpose_null_is_an_error() {
        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        get_payment_form(
            &mut env,
            Some(td_api::InputInvoice::inputInvoiceTelegram(
                td_api::inputInvoiceTelegram { purpose: None },
            )),
            None,
            promise,
        );
        assert_eq!(
            recorded.error(),
            Some((400, "Purpose must be non-empty".to_owned()))
        );
    }

    // --- the giveaway-plane tests ---

    fn giveaway_parameters() -> td_api::GiveawayParameters {
        td_api::GiveawayParameters {
            boosted_chat_id: 0,
            additional_chat_ids: Vec::new(),
            winners_selection_date: 0,
            only_new_members: true,
            has_public_winners: false,
            country_codes: Vec::new(),
            prize_description: String::new(),
        }
    }

    fn giveaway_arm(
        env: &mut TestEnv,
        parameters: Option<td_api::GiveawayParameters>,
    ) -> Recorded<td_api::PaymentForm> {
        let (promise, recorded) = recording_promise();
        get_payment_form(
            env,
            Some(td_api::InputInvoice::inputInvoiceTelegram(
                td_api::inputInvoiceTelegram {
                    purpose: Some(
                        td_api::TelegramPaymentPurpose::telegramPaymentPurposePremiumGiveaway(
                            td_api::telegramPaymentPurposePremiumGiveaway {
                                parameters,
                                currency: "USD".to_owned(),
                                amount: 100,
                                winner_count: 3,
                                month_count: 1,
                            },
                        ),
                    ),
                },
            )),
            None,
            promise,
        );
        recorded
    }

    #[test]
    fn giveaway_parameters_null_is_an_error() {
        let mut env = TestEnv::default();
        let recorded = giveaway_arm(&mut env, None);
        assert_eq!(
            recorded.error(),
            Some((400, "Giveaway parameters must be non-empty".to_owned()))
        );
    }

    #[test]
    fn giveaway_parameters_gate_on_the_boosted_channel() {
        // the boosted channel is unknown → "Chat to boost not found"
        let mut env = TestEnv::default();
        let recorded = giveaway_arm(&mut env, Some(giveaway_parameters()));
        assert_eq!(
            recorded.error(),
            Some((400, "Chat to boost not found".to_owned()))
        );

        // a known channel with a stale date → "Giveaway date is in the past"
        let channel_dialog = DialogId::from_channel_id(ChannelId::new(1000));
        let mut env = TestEnv::default();
        env.have_dialogs.push(channel_dialog.get());
        let mut parameters = giveaway_parameters();
        parameters.boosted_chat_id = channel_dialog.get();
        parameters.winners_selection_date = env.unix_time - 1;
        let recorded = giveaway_arm(&mut env, Some(parameters));
        assert_eq!(
            recorded.error(),
            Some((400, "Giveaway date is in the past".to_owned()))
        );

        // the too-many-additional-chats option gate
        let mut env = TestEnv::default();
        env.have_dialogs.push(channel_dialog.get());
        env.options
            .insert("giveaway_additional_chat_count_max".to_owned(), 0);
        let mut parameters = giveaway_parameters();
        parameters.boosted_chat_id = channel_dialog.get();
        parameters.additional_chat_ids = vec![channel_dialog.get()];
        parameters.winners_selection_date = env.unix_time + 100;
        let recorded = giveaway_arm(&mut env, Some(parameters));
        assert_eq!(
            recorded.error(),
            Some((400, "Too many additional chats specified".to_owned()))
        );

        // the country-code shape gate
        let mut env = TestEnv::default();
        env.have_dialogs.push(channel_dialog.get());
        let mut parameters = giveaway_parameters();
        parameters.boosted_chat_id = channel_dialog.get();
        parameters.winners_selection_date = env.unix_time + 100;
        parameters.country_codes = vec!["de".to_owned()];
        let recorded = giveaway_arm(&mut env, Some(parameters));
        assert_eq!(
            recorded.error(),
            Some((400, "Invalid country code specified".to_owned()))
        );

        // the non-admin gate (the broadcast channel requires can_post)
        let mut env = TestEnv::default();
        env.have_dialogs.push(channel_dialog.get());
        env.channel_admin = false;
        let mut parameters = giveaway_parameters();
        parameters.boosted_chat_id = channel_dialog.get();
        parameters.winners_selection_date = env.unix_time + 100;
        let recorded = giveaway_arm(&mut env, Some(parameters));
        assert_eq!(
            recorded.error(),
            Some((400, "Not enough rights in the chat".to_owned()))
        );

        // the happy path reaches the net seam
        let mut env = TestEnv::default();
        env.have_dialogs.push(channel_dialog.get());
        let mut parameters = giveaway_parameters();
        parameters.boosted_chat_id = channel_dialog.get();
        parameters.winners_selection_date = env.unix_time + 100;
        let recorded = giveaway_arm(&mut env, Some(parameters));
        assert!(recorded.pending(), "the net seam parks the promise");
        assert_eq!(env.queries, vec!["getPaymentForm:false"]);
    }

    #[test]
    fn gift_codes_purpose_needs_an_accessible_boosted_chat() {
        let gift_codes = |boosted_chat_id: i64| {
            td_api::InputInvoice::inputInvoiceTelegram(td_api::inputInvoiceTelegram {
                purpose: Some(
                    td_api::TelegramPaymentPurpose::telegramPaymentPurposePremiumGiftCodes(
                        td_api::telegramPaymentPurposePremiumGiftCodes {
                            boosted_chat_id,
                            currency: "USD".to_owned(),
                            amount: 100,
                            user_ids: vec![1, 2],
                            month_count: 1,
                            text: None,
                        },
                    ),
                ),
            })
        };

        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        get_payment_form(&mut env, Some(gift_codes(-1000000000002)), None, promise);
        assert_eq!(
            recorded.error(),
            Some((400, "Chat to boost not found".to_owned()))
        );

        // with the channel known and admin, the flow reaches the seam
        let mut env = TestEnv::default();
        env.have_dialogs.push(-1000000000002);
        let (promise, _recorded) = recording_promise();
        get_payment_form(&mut env, Some(gift_codes(-1000000000002)), None, promise);
        assert_eq!(env.queries, vec!["getPaymentForm:false"]);
    }

    // --- the ResultHandler answer tests ---

    fn wire_payment_form() -> telegram_api::payments_paymentForm {
        telegram_api::payments_paymentForm {
            flags: 0,
            can_save_credentials: true,
            password_missing: false,
            form_id: 1234,
            bot_id: 42,
            title: "Test product".to_owned(),
            description: "A long @description".to_owned(),
            photo: None,
            invoice: Some(wire_invoice()),
            provider_id: 43,
            url: "https://pay.example.com".to_owned(),
            native_provider: String::new(),
            native_params: None,
            additional_methods: vec![telegram_api::PaymentFormMethod {
                title: "Wallet".to_owned(),
                url: "https://wallet.example.com".to_owned(),
            }],
            saved_info: None,
            saved_credentials: Vec::new(),
            users: Vec::new(),
        }
    }

    #[test]
    fn payment_form_answer_builds_the_regular_object() {
        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        on_get_payment_form_result(&mut env, DialogId::new(100), wire_payment_form(), promise);
        let form = recorded.value().expect("payment form");
        assert_eq!(form.id, 1234);
        assert_eq!(form.seller_bot_user_id, 42);
        let product_info = form.product_info.expect("product info");
        assert_eq!(product_info.title, "Test product");
        let description = product_info.description.expect("description");
        assert!(!description.entities.is_empty(), "the mention is found");
        let Some(td_api::PaymentFormType::paymentFormTypeRegular(regular)) = form.r#type else {
            panic!("wrong form type");
        };
        assert_eq!(regular.payment_provider_user_id, 43);
        assert!(regular.can_save_credentials);
        assert!(!regular.need_password);
        let provider = regular.payment_provider.expect("provider");
        assert!(matches!(
            provider,
            td_api::PaymentProvider::paymentProviderOther(_)
        ));
        assert_eq!(regular.additional_payment_options.len(), 1);
        let invoice = regular.invoice.expect("invoice");
        assert!(invoice.is_test);
        assert!(invoice.need_phone_number);
        assert_eq!(env.users_intake, vec!["GetPaymentFormQuery 1:0"]);
        assert_eq!(env.user_id_objects, vec![43, 42]);
    }

    #[test]
    fn payment_form_answer_validates_the_bot_ids() {
        let mut env = TestEnv::default();
        let mut wire = wire_payment_form();
        wire.provider_id = 0;
        let (promise, recorded) = recording_promise();
        on_get_payment_form_result(&mut env, DialogId::new(100), wire, promise);
        assert_eq!(
            recorded.error(),
            Some((
                500,
                "Receive invalid payments provider identifier".to_owned()
            ))
        );

        let mut env = TestEnv::default();
        let mut wire = wire_payment_form();
        wire.bot_id = 0;
        let (promise, recorded) = recording_promise();
        on_get_payment_form_result(&mut env, DialogId::new(100), wire, promise);
        assert_eq!(
            recorded.error(),
            Some((500, "Receive invalid seller identifier".to_owned()))
        );
    }

    #[test]
    fn payment_form_stars_answer_builds_both_subscription_shapes() {
        let wire = |subscription_period: i32| telegram_api::payments_paymentFormStars {
            flags: 0,
            form_id: 555,
            bot_id: 42,
            title: "Stars".to_owned(),
            description: "Stars product".to_owned(),
            photo: None,
            invoice: Some(telegram_api::Invoice {
                prices: vec![telegram_api::LabeledPrice {
                    label: "stars".to_owned(),
                    amount: 250,
                }],
                subscription_period,
                ..Default::default()
            }),
            users: Vec::new(),
        };

        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        on_get_payment_form_stars_result(&mut env, DialogId::new(100), wire(0), promise);
        let form = recorded.value().expect("payment form");
        let Some(td_api::PaymentFormType::paymentFormTypeStars(stars)) = form.r#type else {
            panic!("wrong form type");
        };
        assert_eq!(stars.star_count, 250);

        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        on_get_payment_form_stars_result(&mut env, DialogId::new(100), wire(2592000), promise);
        let form = recorded.value().expect("payment form");
        let Some(td_api::PaymentFormType::paymentFormTypeStarSubscription(subscription)) =
            form.r#type
        else {
            panic!("wrong form type");
        };
        let pricing = subscription.pricing.expect("pricing");
        assert_eq!(pricing.period, 2592000);
        assert_eq!(pricing.star_count, 250);
        assert_eq!(env.users_intake, vec!["GetPaymentFormQuery 2:0"]);

        // the single-price gate
        let mut env = TestEnv::default();
        let mut wire = wire(0);
        wire.invoice
            .as_mut()
            .unwrap()
            .prices
            .push(telegram_api::LabeledPrice {
                label: "extra".to_owned(),
                amount: 1,
            });
        let (promise, recorded) = recording_promise();
        on_get_payment_form_stars_result(&mut env, DialogId::new(100), wire, promise);
        assert_eq!(
            recorded.error(),
            Some((500, "Receive invalid price".to_owned()))
        );
    }

    #[test]
    fn validated_order_info_answer_converts_the_options() {
        let wire = telegram_api::payments_ValidatedRequestedInfo {
            flags: 0,
            id: "order-1".to_owned(),
            shipping_options: vec![telegram_api::ShippingOption {
                id: "opt".to_owned(),
                title: "Post".to_owned(),
                prices: vec![telegram_api::LabeledPrice {
                    label: "fee".to_owned(),
                    amount: 9,
                }],
            }],
        };
        let (promise, recorded) = recording_promise();
        on_get_validated_order_info_result(wire, promise);
        let order = recorded.value().expect("validated order info");
        assert_eq!(order.order_info_id, "order-1");
        assert_eq!(order.shipping_options.len(), 1);
        assert_eq!(order.shipping_options[0].price_parts[0].amount, 9);
    }

    /// The env whose `on_get_updates` completes immediately — the
    /// payments_paymentResult tail's continuation contract.
    struct ImmediateUpdates;

    impl PaymentsEnv for ImmediateUpdates {
        fn get_unix_time(&self) -> i32 {
            0
        }
        fn get_option_integer(&mut self, _name: &str) -> i64 {
            0
        }
        fn binlog_pmc_get_raw(&mut self, _key: &[u8]) -> Vec<u8> {
            Vec::new()
        }
        fn get_input_user(&mut self, _user_id: UserId) -> Result<telegram_api::InputUser> {
            unimplemented!()
        }
        fn get_input_users(
            &mut self,
            _user_ids: &[UserId],
        ) -> Result<Vec<telegram_api::InputUser>> {
            unimplemented!()
        }
        fn on_get_users(&mut self, _users: Vec<telegram_api::User>, _source: &str) {}
        fn get_user_id_object(&mut self, user_id: UserId, _source: &str) -> i64 {
            user_id.get()
        }
        fn have_dialog_force(&mut self, _dialog_id: DialogId, _source: &str) -> bool {
            false
        }
        fn get_channel_status(&mut self, _channel_id: ChannelId) -> DialogParticipantStatus {
            DialogParticipantStatus::banned(0, String::new())
        }
        fn is_broadcast_channel(&mut self, _channel_id: ChannelId) -> bool {
            false
        }
        fn get_input_peer(
            &mut self,
            _dialog_id: DialogId,
            _access_rights: AccessRights,
        ) -> Option<telegram_api::InputPeer> {
            None
        }
        fn check_dialog_access(&mut self, _dialog_id: DialogId, _source: &str) -> Result<()> {
            Result::Ok(())
        }
        fn get_my_dialog_id(&self) -> DialogId {
            DialogId::new(0)
        }
        fn on_get_updates(
            &mut self,
            _updates: telegram_api::Updates,
            mut on_complete: Promise<()>,
        ) {
            on_complete.set_value(());
        }
    }

    #[test]
    fn send_payment_form_result_answers_directly_or_through_the_updates_tail() {
        // the verification-needed answer lands immediately
        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        on_get_send_payment_form_result(
            &mut env,
            telegram_api::payments_PaymentResult::payments_paymentVerificationNeeded(
                telegram_api::payments_paymentVerificationNeeded {
                    url: "https://verify.example.com".to_owned(),
                },
            ),
            promise,
            false,
        );
        let result = recorded.value().expect("payment result");
        assert!(!result.success);
        assert_eq!(result.verification_url, "https://verify.example.com");

        // the payments_paymentResult tail completes with paymentResult(true,
        // "") once the updates are applied (Payments.cpp:687-694)
        let mut env = ImmediateUpdates;
        let (promise, recorded) = recording_promise();
        on_get_send_payment_form_result(
            &mut env,
            telegram_api::payments_PaymentResult::payments_paymentResult(
                telegram_api::payments_paymentResult { updates: None },
            ),
            promise,
            true,
        );
        let result = recorded.value().expect("payment result");
        assert!(result.success);
        assert_eq!(result.verification_url, "");
    }

    #[test]
    fn payment_receipt_answers_build_both_objects() {
        let wire_stars = telegram_api::payments_paymentReceiptStars {
            flags: 0,
            date: 1700000000,
            bot_id: 42,
            title: "Stars receipt".to_owned(),
            description: "Receipt".to_owned(),
            photo: None,
            invoice: Some(telegram_api::Invoice {
                prices: vec![telegram_api::LabeledPrice {
                    label: "stars".to_owned(),
                    amount: 500,
                }],
                ..Default::default()
            }),
            currency: "XTR".to_owned(),
            total_amount: 500,
            transaction_id: "tx1".to_owned(),
            users: Vec::new(),
        };
        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        on_get_payment_receipt_stars_result(&mut env, DialogId::new(100), wire_stars, promise);
        let receipt = recorded.value().expect("receipt");
        assert_eq!(receipt.date, 1700000000);
        assert_eq!(receipt.seller_bot_user_id, 42);
        let Some(td_api::PaymentReceiptType::paymentReceiptTypeStars(stars)) = receipt.r#type
        else {
            panic!("wrong receipt type");
        };
        assert_eq!(stars.star_count, 500);
        assert_eq!(stars.transaction_id, "tx1");

        let wire_regular = telegram_api::payments_paymentReceipt {
            flags: 0,
            date: 1700000001,
            bot_id: 42,
            provider_id: 43,
            title: "Regular receipt".to_owned(),
            description: "Receipt".to_owned(),
            photo: None,
            invoice: Some(wire_invoice()),
            info: None,
            shipping: None,
            tip_amount: 5,
            currency: "USD".to_owned(),
            total_amount: 110,
            credentials_title: "Visa".to_owned(),
            users: Vec::new(),
        };
        let mut env = TestEnv::default();
        let (promise, recorded) = recording_promise();
        on_get_payment_receipt_result(&mut env, DialogId::new(100), wire_regular, promise);
        let receipt = recorded.value().expect("receipt");
        let Some(td_api::PaymentReceiptType::paymentReceiptTypeRegular(regular)) = receipt.r#type
        else {
            panic!("wrong receipt type");
        };
        assert_eq!(regular.payment_provider_user_id, 43);
        assert_eq!(regular.credentials_title, "Visa");
        assert_eq!(regular.tip_amount, 5);
        assert!(regular.invoice.is_some());
        assert_eq!(env.user_id_objects, vec![42, 43]);
    }

    #[test]
    fn payment_receipt_tip_amount_clamps() {
        let mut env = TestEnv::default();
        let wire = telegram_api::payments_paymentReceipt {
            flags: 0,
            date: 1,
            bot_id: 42,
            provider_id: 43,
            title: "T".to_owned(),
            description: "D".to_owned(),
            photo: None,
            invoice: Some(wire_invoice()),
            info: None,
            shipping: None,
            tip_amount: -(1 << 41),
            currency: "USD".to_owned(),
            total_amount: 1,
            credentials_title: String::new(),
            users: Vec::new(),
        };
        let (promise, recorded) = recording_promise();
        on_get_payment_receipt_result(&mut env, DialogId::new(100), wire, promise);
        let receipt = recorded.value().expect("receipt");
        let Some(td_api::PaymentReceiptType::paymentReceiptTypeRegular(regular)) = receipt.r#type
        else {
            panic!("wrong receipt type");
        };
        assert_eq!(regular.tip_amount, 0);
    }

    #[test]
    fn saved_info_exported_invoice_bank_card_and_collectible_answers() {
        let (promise, recorded) = recording_promise();
        on_get_saved_order_info_result(
            telegram_api::payments_SavedInfo {
                flags: 0,
                has_saved_credentials: false,
                saved_info: None,
            },
            promise,
        );
        recorded.value().expect("order info");

        let (promise, recorded) = recording_promise();
        on_get_exported_invoice_result(
            telegram_api::payments_ExportedInvoice {
                url: "https://t.me/invoice".to_owned(),
            },
            promise,
        );
        assert_eq!(recorded.value(), Some("https://t.me/invoice".to_owned()));

        let (promise, recorded) = recording_promise();
        on_get_bank_card_info_result(
            telegram_api::payments_BankCardData {
                title: "Visa".to_owned(),
                open_urls: vec![telegram_api::BankCardOpenUrl {
                    name: "Bank".to_owned(),
                    url: "https://bank.example.com".to_owned(),
                }],
            },
            promise,
        );
        let info = recorded.value().expect("bank card info");
        assert_eq!(info.title, "Visa");
        assert_eq!(info.actions.len(), 1);
        assert_eq!(info.actions[0].text, "Bank");

        let (promise, recorded) = recording_promise();
        on_get_collectible_info_result(
            telegram_api::fragment_CollectibleInfo {
                purchase_date: 1700000000,
                currency: "TON".to_owned(),
                amount: 123,
                crypto_currency: "TON".to_owned(),
                crypto_amount: 123,
                url: "https://fragment.com".to_owned(),
            },
            promise,
        );
        let info = recorded.value().expect("collectible info");
        assert_eq!(info.currency, "TON");
        assert_eq!(info.cryptocurrency_amount, 123);

        // the invalid price halves zero out
        let (promise, recorded) = recording_promise();
        on_get_collectible_info_result(
            telegram_api::fragment_CollectibleInfo {
                purchase_date: 1,
                currency: String::new(),
                amount: -5,
                crypto_currency: String::new(),
                crypto_amount: -5,
                url: String::new(),
            },
            promise,
        );
        let info = recorded.value().expect("collectible info");
        assert_eq!(info.amount, 0);
        assert_eq!(info.cryptocurrency_amount, 0);
    }

    #[test]
    fn product_info_photo_seam_and_input_media_wire_shape() {
        let mut env = TestEnv {
            photo_object: Some(td_api::Photo::default()),
            ..TestEnv::default()
        };
        let info = get_product_info_object(&mut env, "Title", "Hello", &Photo::default());
        assert_eq!(info.title, "Title");
        assert!(info.photo.is_some());

        let mut env = TestEnv::default();
        let info = get_product_info_object(&mut env, "Title", "Hi", &Photo::default());
        assert!(info.photo.is_none());

        // the photo-less, extended-media-less invoice media carries the
        // START_PARAM + PROVIDER masks and the "null" provider data
        let invoice = InputInvoice {
            title: "T".to_owned(),
            description: "D".to_owned(),
            start_parameter: "start".to_owned(),
            payload: b"payload".to_vec(),
            provider_token: "tok".to_owned(),
            provider_data: String::new(),
            invoice: Invoice {
                currency: "USD".to_owned(),
                price_parts: vec![LabeledPricePart {
                    label: "base".to_owned(),
                    amount: 100,
                }],
                ..Default::default()
            },
            ..InputInvoice::default()
        };
        let media = invoice.get_input_media_invoice(&mut env).expect("media");
        assert_eq!(
            media.flags,
            INPUT_MEDIA_INVOICE_START_PARAM_MASK | INPUT_MEDIA_INVOICE_PROVIDER_MASK
        );
        assert_eq!(media.provider, "tok");
        assert_eq!(media.provider_data.expect("provider data").data, "null");
        assert_eq!(media.start_param, "start");
        assert_eq!(media.payload, b"payload");
        let media_invoice = media.invoice.expect("invoice");
        assert_eq!(media_invoice.prices.len(), 1);
        assert_eq!(media_invoice.prices[0].amount, 100);
    }

    #[test]
    fn business_connection_id_validity() {
        let empty = BusinessConnectionId::default();
        assert!(!empty.is_valid());
        let id = BusinessConnectionId::new("conn".to_owned());
        assert!(id.is_valid());
        assert_eq!(id.get(), "conn");
    }
}
