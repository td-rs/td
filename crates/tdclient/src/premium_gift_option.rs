//! `PremiumGiftOption` (td/telegram/PremiumGiftOption.{h,cpp,hpp}) — the
//! M.36 Wave-3 premium gift-option value class. The bot-url internal-link
//! parse is the recorded seam (the `LinkManager::parse_internal_link`
//! surface is Wave-2 M.23) — the link object answers `None`.

use crate::store_parse::{self, Parser};
use crate::td_api;
use tdmtproto::telegram_api;

/// `td::PremiumGiftOption` — a premium subscription option.
#[derive(Clone, Debug, Default)]
pub struct PremiumGiftOption {
    pub months: i32,
    pub is_current: bool,
    pub is_upgrade: bool,
    pub currency: String,
    pub amount: i64,
    pub bot_url: String,
    pub store_product: String,
    pub transaction: String,
}

impl PremiumGiftOption {
    /// The wire ctor (PremiumGiftOption.cpp:7-17).
    pub fn from_telegram(option: &telegram_api::PremiumSubscriptionOption) -> PremiumGiftOption {
        PremiumGiftOption {
            months: option.months,
            is_current: option.current,
            is_upgrade: option.can_purchase_upgrade,
            currency: option.currency.clone(),
            amount: option.amount,
            bot_url: option.bot_url.clone(),
            store_product: option.store_product.clone(),
            transaction: option.transaction.clone(),
        }
    }

    /// `is_valid` (PremiumGiftOption.cpp:18-28).
    pub fn is_valid(&self) -> bool {
        if self.amount <= 0 || !crate::misc::check_currency_amount(self.amount) {
            tdutils::log_error!(
                "Receive invalid premium payment option amount {}",
                self.amount
            );
            return false;
        }
        if self.currency.len() != 3 {
            tdutils::log_error!(
                "Receive invalid premium payment option currency {}",
                self.currency
            );
            return false;
        }
        true
    }

    /// `get_monthly_price` (PremiumGiftOption.cpp:29-31).
    fn get_monthly_price(&self) -> f64 {
        self.amount as f64 / self.months as f64
    }

    /// `get_premium_payment_option_object` (PremiumGiftOption.cpp:32-45).
    pub fn get_premium_payment_option_object(
        &self,
        base_option: &PremiumGiftOption,
    ) -> td_api::PremiumPaymentOption {
        // `LinkManager::parse_internal_link(bot_url_, true)` — the M.23
        // seam; the link object answers None.
        let mut discount_percentage = 0;
        if base_option.months > 0 && self.months > 0 && base_option.amount > 0 && self.amount > 0 {
            let relative_price = self.get_monthly_price() / base_option.get_monthly_price();
            if relative_price < 1.0 {
                discount_percentage = (100.0 * (1.0 - relative_price)) as i32;
            }
        }
        td_api::PremiumPaymentOption {
            currency: self.currency.clone(),
            amount: self.amount,
            discount_percentage,
            month_count: self.months,
            store_product_id: self.store_product.clone(),
            payment_link: None,
        }
    }

    /// `get_premium_state_payment_option_object`
    /// (PremiumGiftOption.cpp:46-49).
    pub fn get_premium_state_payment_option_object(
        &self,
        base_option: &PremiumGiftOption,
    ) -> td_api::PremiumStatePaymentOption {
        td_api::PremiumStatePaymentOption {
            payment_option: Some(self.get_premium_payment_option_object(base_option)),
            is_current: self.is_current,
            is_upgrade: self.is_upgrade,
            last_transaction_id: self.transaction.clone(),
        }
    }

    /// `PremiumGiftOption::store` (PremiumGiftOption.hpp).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags(out, [self.is_current, self.is_upgrade]);
        store_parse::store_i32(out, self.months);
        store_parse::store_string(out, &self.currency);
        store_parse::store_i64(out, self.amount);
        store_parse::store_string(out, &self.bot_url);
        store_parse::store_string(out, &self.store_product);
        store_parse::store_string(out, &self.transaction);
    }

    /// `PremiumGiftOption::parse` (PremiumGiftOption.hpp).
    pub fn parse(parser: &mut Parser) -> Result<PremiumGiftOption, &'static str> {
        let [is_current, is_upgrade] = parser.parse_flags()?;
        Ok(PremiumGiftOption {
            months: parser.i32_()?,
            currency: parser.string()?,
            amount: parser.i64_()?,
            bot_url: parser.string()?,
            store_product: parser.string()?,
            transaction: parser.string()?,
            is_current,
            is_upgrade,
        })
    }
}

impl PartialEq for PremiumGiftOption {
    fn eq(&self, other: &Self) -> bool {
        self.months == other.months
            && self.currency == other.currency
            && self.amount == other.amount
            && self.bot_url == other.bot_url
            && self.store_product == other.store_product
    }
}

impl Eq for PremiumGiftOption {}

impl PartialOrd for PremiumGiftOption {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PremiumGiftOption {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (
            self.months,
            self.amount,
            &self.currency,
            &self.store_product,
            &self.bot_url,
        )
            .cmp(&(
                other.months,
                other.amount,
                &other.currency,
                &other.store_product,
                &other.bot_url,
            ))
    }
}

/// `get_premium_gift_options` (PremiumGiftOption.cpp:60-66).
pub fn get_premium_gift_options(
    options: &[telegram_api::PremiumSubscriptionOption],
) -> Vec<PremiumGiftOption> {
    options
        .iter()
        .map(PremiumGiftOption::from_telegram)
        .filter(PremiumGiftOption::is_valid)
        .collect()
}

/// `get_premium_state_payment_options_object` (PremiumGiftOption.cpp:67-75).
pub fn get_premium_state_payment_options_object(
    options: &[PremiumGiftOption],
) -> Vec<td_api::PremiumStatePaymentOption> {
    if options.is_empty() {
        return Vec::new();
    }
    let base_premium_option = options.iter().min().expect("options are non-empty");
    options
        .iter()
        .map(|option| option.get_premium_state_payment_option_object(base_premium_option))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire_option(months: i32, amount: i64) -> telegram_api::PremiumSubscriptionOption {
        telegram_api::PremiumSubscriptionOption {
            flags: 0,
            current: false,
            can_purchase_upgrade: false,
            months,
            currency: "USD".to_string(),
            amount,
            bot_url: "https://t.me/premiumbot".to_string(),
            store_product: "product".to_string(),
            transaction: String::new(),
        }
    }

    #[test]
    fn validity_checks_amount_and_currency() {
        assert!(!PremiumGiftOption::from_telegram(&wire_option(1, 0)).is_valid());
        assert!(
            !PremiumGiftOption::from_telegram(&telegram_api::PremiumSubscriptionOption {
                currency: "US".to_string(),
                ..wire_option(1, 100)
            })
            .is_valid()
        );
        assert!(PremiumGiftOption::from_telegram(&wire_option(1, 100)).is_valid());
    }

    #[test]
    fn discount_comes_from_the_monthly_price_ratio() {
        let base = PremiumGiftOption::from_telegram(&wire_option(1, 100));
        let cheaper = PremiumGiftOption::from_telegram(&wire_option(12, 600));
        let object = cheaper.get_premium_payment_option_object(&base);
        assert_eq!(object.discount_percentage, 50);
        assert_eq!(object.month_count, 12);
    }

    #[test]
    fn store_parse_round_trips() {
        let option = PremiumGiftOption::from_telegram(&wire_option(6, 500));
        let mut data = Vec::new();
        store_parse::store_log_event_version(&mut data);
        option.store(&mut data);
        let mut parser = Parser::new_log_event(&data).unwrap();
        let parsed = PremiumGiftOption::parse(&mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed, option);
    }

    #[test]
    fn the_state_object_uses_the_cheapest_base() {
        let options = get_premium_gift_options(&[wire_option(1, 100), wire_option(12, 600)]);
        let objects = get_premium_state_payment_options_object(&options);
        assert_eq!(objects.len(), 2);
        assert_eq!(
            objects[1]
                .payment_option
                .as_ref()
                .unwrap()
                .discount_percentage,
            50
        );
    }
}
