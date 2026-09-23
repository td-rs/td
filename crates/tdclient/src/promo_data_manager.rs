//! Port of `td/telegram/PromoDataManager.{h,cpp}` (M.26.1) — the
//! promo-data plane: the auth-gated init, the getPromoData reload
//! machine with the expiry re-schedule arithmetic, the `on_get_promo_data`
//! local intake halves (users/chats, the sponsored-dialog source, the
//! dismissed-birthday option machine, the suggested-action table) and the
//! hide tail.
//!
//! The manager is the collapsed actor: `Td` owns the struct, the
//! [`PromoDataManagerEnv`] trait carries the `G()`/option/cross-manager
//! surfaces, and the two network tails are env seams marked
//! `[blocked: net]` — `GetPromoDataQuery` (PromoDataManager.cpp:31-56)
//! and `HidePromoDataQuery` (PromoDataManager.cpp:58-86). The get
//! answer arrives through [`PromoDataManager::on_get_promo_data`] once
//! the net step lands (the C++ `PromiseCreator::lambda` hop), so no
//! promise is parked here.
//!
//! The C++ actor timeout (`set_timeout_in`, PromoDataManager.cpp:126)
//! is the single absolute deadline [`PromoDataManager::next_deadline`]
//! exposes; `Td` merges it into its timeout slot and the fired timeout
//! runs [`PromoDataManager::timeout_expired`] — the MultiTimeout
//! convention of the collapsed line.
//!
//! Seamed intakes (the local halves stay here, the consumers are not
//! landed): `MessagesManager::set_sponsored_dialog` is the M.10 seam,
//! `SuggestedActionManager::update_suggested_actions` is the Wave-3
//! seam, and the custom suggestion's `get_formatted_text` title/
//! description parse rides the M.9 entities plane — the raw strings are
//! kept (documented divergence; the emptiness gates are byte-exact).

use crate::dialog_id::DialogId;
use tdmtproto::telegram_api;
use tdutils::status::Result;

/// The `DialogSource` slice the promo intake produces (DialogSource.h) —
/// the empty/membership default, the mtproto-proxy marker and the
/// public-service-announcement pair. The full plane (serialize/
/// unserialize/`get_chat_source_object`) stays with the DialogSource.h
/// owner; nothing else in this track consumes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DialogSource {
    #[default]
    Membership,
    MtprotoProxy,
    PublicServiceAnnouncement {
        psa_type: String,
        psa_text: String,
    },
}

impl DialogSource {
    /// `DialogSource::mtproto_proxy()` (DialogSource.cpp).
    pub fn mtproto_proxy() -> DialogSource {
        DialogSource::MtprotoProxy
    }

    /// `DialogSource::public_service_announcement(psa_type, psa_text)`.
    pub fn public_service_announcement(psa_type: String, psa_text: String) -> DialogSource {
        DialogSource::PublicServiceAnnouncement { psa_type, psa_text }
    }
}

/// The `SuggestedAction::Type` slice the promo intake can produce
/// (SuggestedAction.h:25-45): the empty default plus the seventeen
/// string-table entries and the custom arm.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SuggestedActionType {
    #[default]
    Empty,
    EnableArchiveAndMuteNewChats,
    CheckPhoneNumber,
    ViewChecksHint,
    ConvertToGigagroup,
    CheckPassword,
    SetPassword,
    UpgradePremium,
    SubscribeToAnnualPremium,
    RestorePremium,
    GiftPremiumForChristmas,
    BirthdaySetup,
    PremiumGrace,
    StarsSubscriptionLowBalance,
    UserpicSetup,
    Custom,
    SetupLoginEmail,
    SetupLoginEmailNoskip,
    SetupPasskey,
}

/// The `SuggestedAction` slice the promo intake builds (the
/// `SuggestedAction(Slice)` string table and the
/// `SuggestedAction(pendingSuggestion)` custom arm). The dialog-id and
/// relogin-days arms belong to other constructors' callers; the
/// title/description `get_formatted_text` parse is the M.9 entities
/// seam — the raw strings are kept, the emptiness gates are exact.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SuggestedAction {
    action_type: SuggestedActionType,
    custom_type: String,
}

impl SuggestedAction {
    /// `SuggestedAction::SuggestedAction(Slice action_str)`
    /// (SuggestedAction.cpp:29-63) — the wire-name table, in order.
    pub fn from_action_str(action_str: &str) -> SuggestedAction {
        let action_type = match action_str {
            "AUTOARCHIVE_POPULAR" => SuggestedActionType::EnableArchiveAndMuteNewChats,
            "VALIDATE_PASSWORD" => SuggestedActionType::CheckPassword,
            "VALIDATE_PHONE_NUMBER" => SuggestedActionType::CheckPhoneNumber,
            "NEWCOMER_TICKS" => SuggestedActionType::ViewChecksHint,
            "SETUP_PASSWORD" => SuggestedActionType::SetPassword,
            "PREMIUM_UPGRADE" => SuggestedActionType::UpgradePremium,
            "PREMIUM_ANNUAL" => SuggestedActionType::SubscribeToAnnualPremium,
            "PREMIUM_RESTORE" => SuggestedActionType::RestorePremium,
            "PREMIUM_CHRISTMAS" => SuggestedActionType::GiftPremiumForChristmas,
            "BIRTHDAY_SETUP" => SuggestedActionType::BirthdaySetup,
            "PREMIUM_GRACE" => SuggestedActionType::PremiumGrace,
            "STARS_SUBSCRIPTION_LOW_BALANCE" => SuggestedActionType::StarsSubscriptionLowBalance,
            "USERPIC_SETUP" => SuggestedActionType::UserpicSetup,
            "SETUP_LOGIN_EMAIL" => SuggestedActionType::SetupLoginEmail,
            "SETUP_LOGIN_EMAIL_NOSKIP" => SuggestedActionType::SetupLoginEmailNoskip,
            "SETUP_PASSKEY" => SuggestedActionType::SetupPasskey,
            // an unparsed string keeps the C++ empty default
            _ => SuggestedActionType::Empty,
        };
        SuggestedAction {
            action_type,
            custom_type: String::new(),
        }
    }

    /// `SuggestedAction::SuggestedAction(const UserManager *,
    /// pendingSuggestion)` (SuggestedAction.cpp:65-76) — the
    /// suggestion/url emptiness gates are byte-exact; the title/
    /// description `get_formatted_text` parse is the M.9 seam and keeps
    /// the raw strings.
    pub fn from_pending_suggestion(action: &telegram_api::PendingSuggestion) -> SuggestedAction {
        if action.suggestion.is_empty() || action.url.is_empty() {
            return SuggestedAction::default();
        }
        SuggestedAction {
            action_type: SuggestedActionType::Custom,
            custom_type: action.suggestion.clone(),
        }
    }

    /// `is_empty` (SuggestedAction.h:78-80).
    pub fn is_empty(&self) -> bool {
        self.action_type == SuggestedActionType::Empty
    }

    /// The action type (the local accessor of the private C++ field).
    pub fn get_action_type(&self) -> SuggestedActionType {
        self.action_type
    }
}

/// The `Td *td` slice of the collapsed PromoDataManager.
pub trait PromoDataManagerEnv {
    // -- the G()/auth slice
    fn close_flag(&self) -> bool;
    fn auth_is_authorized(&self) -> bool;
    fn auth_is_bot(&self) -> bool;
    /// the actor-timeout clock (`set_timeout_in` basis)
    fn now(&self) -> f64;
    /// `G()->unix_time()`
    fn unix_time(&self) -> i32;

    // -- the option slice
    fn get_option_integer(&self, name: &str, default_value: i64) -> i64;
    fn set_option_boolean(&mut self, name: &str, value: bool);
    fn set_option_empty(&mut self, name: &str);

    // -- the cross-manager intakes
    /// `UserManager::on_get_users`.
    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &'static str);
    /// `ChatManager::on_get_chats`.
    fn on_get_chats(&mut self, chats: Vec<telegram_api::Chat>, source: &'static str);

    // -- the seamed cross-manager surfaces
    /// `MessagesManager::set_sponsored_dialog` — [blocked: M.10].
    fn set_sponsored_dialog(&mut self, dialog_id: DialogId, source: DialogSource);
    /// `SuggestedActionManager::update_suggested_actions` — the
    /// suggested-actions plane is Wave 3.
    fn update_suggested_actions(&mut self, actions: &[SuggestedAction]);

    // -- the `[blocked: net]` query tails; the get answer routes back
    // -- through `on_get_promo_data` once the net step lands
    /// `GetPromoDataQuery` (PromoDataManager.cpp:31-56).
    fn send_get_promo_data_query(&mut self);
    /// `HidePromoDataQuery` (PromoDataManager.cpp:58-86); the C++ result
    /// is dropped ("we are not interested in the result").
    fn send_hide_promo_data_query(&mut self, dialog_id: DialogId);
}

/// `PromoDataManager` (PromoDataManager.h:21-50) — the collapsed actor.
#[derive(Debug, Default)]
pub struct PromoDataManager {
    is_inited: bool,
    reloading_promo_data: bool,
    need_reload_promo_data: bool,

    /// The armed `set_timeout_in` deadline (absolute, the `now()` clock);
    /// 0.0 = unarmed. The C++ actor timeout with `set_timeout_in(0)`
    /// fires on the next scheduler wakeup — the deadline lands on the
    /// current instant and the next tick picks it up.
    get_promo_data_deadline: f64,
}

impl PromoDataManager {
    pub fn new() -> PromoDataManager {
        PromoDataManager::default()
    }

    /// The earliest armed deadline, for the Td timeout-slot merge.
    pub fn next_deadline(&self) -> Option<f64> {
        if self.get_promo_data_deadline == 0.0 {
            None
        } else {
            Some(self.get_promo_data_deadline)
        }
    }

    /// `init` (PromoDataManager.cpp:99-109); the C++ `start_up`'s
    /// `init()` call (PromoDataManager.cpp:95-97) is the same
    /// idempotent entry point.
    pub fn init(&mut self, env: &mut impl PromoDataManagerEnv) {
        if env.close_flag() {
            return;
        }
        if self.is_inited || !env.auth_is_authorized() || env.auth_is_bot() {
            return;
        }
        self.is_inited = true;

        self.reload_promo_data(env);
    }

    /// `reload_promo_data` (PromoDataManager.cpp:111-117).
    pub fn reload_promo_data(&mut self, env: &mut impl PromoDataManagerEnv) {
        if self.reloading_promo_data {
            self.need_reload_promo_data = true;
            return;
        }
        self.schedule_get_promo_data(env, 0);
    }

    /// `schedule_get_promo_data` (PromoDataManager.cpp:119-127) — the
    /// clamp table: `<= 0` stays 0 (immediate), otherwise `[60, 86400]`.
    fn schedule_get_promo_data(&mut self, env: &impl PromoDataManagerEnv, expires_in: i32) {
        if !self.is_inited {
            return;
        }

        let expires_in = if expires_in <= 0 {
            0
        } else {
            expires_in.clamp(60, 86400)
        };
        self.get_promo_data_deadline = env.now() + f64::from(expires_in);
    }

    /// The expiry-timeout pump — the C++ one-shot actor timeout
    /// (`set_timeout_in`): the deadline disarms first, and
    /// [`PromoDataManager::timeout_expired`] re-arms through
    /// `schedule_get_promo_data` when it re-fetches.
    pub fn tick_timeouts(&mut self, env: &mut impl PromoDataManagerEnv) {
        if let Some(deadline) = self.next_deadline()
            && deadline <= env.now()
        {
            self.get_promo_data_deadline = 0.0;
            self.timeout_expired(env);
        }
    }

    /// `timeout_expired` (PromoDataManager.cpp:129-140). The C++ promise
    /// lambda routes the answer into `on_get_promo_data`; the collapsed
    /// line's net step will call it directly.
    pub fn timeout_expired(&mut self, env: &mut impl PromoDataManagerEnv) {
        if env.close_flag() || !self.is_inited || self.reloading_promo_data {
            return;
        }

        self.reloading_promo_data = true;
        env.send_get_promo_data_query();
    }

    /// `on_get_promo_data` (PromoDataManager.cpp:142-220) — the intake
    /// local halves.
    pub fn on_get_promo_data(
        &mut self,
        env: &mut impl PromoDataManagerEnv,
        r_promo_data: Result<telegram_api::help_PromoData>,
        dummy: bool,
    ) {
        let _ = dummy;
        if env.close_flag() {
            return;
        }
        self.reloading_promo_data = false;

        let promo_data_ptr = match r_promo_data {
            Result::Ok(promo_data_ptr) => promo_data_ptr,
            Result::Err(_) => {
                // the C++ expected-error gate only silences the log
                return self.schedule_get_promo_data(env, 60);
            }
        };
        let mut expires_at;
        match promo_data_ptr {
            telegram_api::help_PromoData::help_promoDataEmpty(promo) => {
                expires_at = promo.expires;
                self.remove_sponsored_dialog(env);
            }
            telegram_api::help_PromoData::help_promoData(promo) => {
                env.on_get_users(promo.users, "on_get_promo_data");
                env.on_get_chats(promo.chats, "on_get_promo_data");
                expires_at = promo.expires;
                if let Some(peer) = promo.peer {
                    let is_proxy = promo.proxy;
                    let source = if is_proxy {
                        DialogSource::mtproto_proxy()
                    } else {
                        DialogSource::public_service_announcement(promo.psa_type, promo.psa_message)
                    };
                    env.set_sponsored_dialog(DialogId::from_peer(&peer), source);
                } else {
                    self.remove_sponsored_dialog(env);
                }
                if promo
                    .dismissed_suggestions
                    .iter()
                    .any(|s| s == "BIRTHDAY_CONTACTS_TODAY")
                {
                    env.set_option_boolean("dismiss_birthday_contact_today", true);
                } else {
                    env.set_option_empty("dismiss_birthday_contact_today");
                }

                let mut suggested_actions = Vec::new();
                for action in &promo.pending_suggestions {
                    let suggested_action = SuggestedAction::from_action_str(action);
                    if !suggested_action.is_empty() {
                        if suggested_action.action_type == SuggestedActionType::SetPassword
                            && env.get_option_integer("otherwise_relogin_days", 0) > 0
                        {
                            // Skip SetPassword suggested action
                        } else {
                            suggested_actions.push(suggested_action);
                        }
                    } else {
                        tdutils::log_error!("Receive unsupported suggested action {action}");
                    }
                }
                if let Some(custom) = &promo.custom_pending_suggestion {
                    let suggested_action = SuggestedAction::from_pending_suggestion(custom);
                    if !suggested_action.is_empty() {
                        suggested_actions.push(suggested_action);
                    } else {
                        tdutils::log_error!("Receive unsupported custom suggested action");
                    }
                }
                env.update_suggested_actions(&suggested_actions);
            }
        }
        if self.need_reload_promo_data {
            self.need_reload_promo_data = false;
            expires_at = 0;
        }
        let delay = if expires_at == 0 {
            0
        } else {
            // the C++ int32 subtraction wraps on extreme expires
            expires_at.wrapping_sub(env.unix_time())
        };
        self.schedule_get_promo_data(env, delay);
    }

    /// `remove_sponsored_dialog` (PromoDataManager.cpp:222-224).
    pub fn remove_sponsored_dialog(&mut self, env: &mut impl PromoDataManagerEnv) {
        env.set_sponsored_dialog(DialogId::default(), DialogSource::default());
    }

    /// `hide_promo_data` (PromoDataManager.cpp:226-230) — the local half
    /// removes the sponsored dialog at once; the caller is
    /// MessagesManager (MessagesManager.cpp:8771, the M.10 seam) and the
    /// query tail is [blocked: net].
    pub fn hide_promo_data(&mut self, env: &mut impl PromoDataManagerEnv, dialog_id: DialogId) {
        self.remove_sponsored_dialog(env);

        env.send_hide_promo_data_query(dialog_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// The recorded-time env over the [`PromoDataManagerEnv`] surfaces.
    struct TempPromoEnv {
        now: f64,
        unix_time: i32,
        is_authorized: bool,
        is_bot: bool,
        close: bool,
        options_integer: HashMap<String, i64>,
        options_boolean: HashMap<String, bool>,
        emptied_options: Vec<String>,
        users_intake: Vec<&'static str>,
        chats_intake: Vec<&'static str>,
        sponsored_dialog: Vec<(DialogId, DialogSource)>,
        suggested_actions: Vec<Vec<SuggestedAction>>,
        get_promo_queries: usize,
        hide_promo_queries: Vec<DialogId>,
    }

    impl TempPromoEnv {
        fn new() -> TempPromoEnv {
            TempPromoEnv {
                now: 1000.0,
                unix_time: 1000,
                is_authorized: true,
                is_bot: false,
                close: false,
                options_integer: HashMap::new(),
                options_boolean: HashMap::new(),
                emptied_options: Vec::new(),
                users_intake: Vec::new(),
                chats_intake: Vec::new(),
                sponsored_dialog: Vec::new(),
                suggested_actions: Vec::new(),
                get_promo_queries: 0,
                hide_promo_queries: Vec::new(),
            }
        }
    }

    impl PromoDataManagerEnv for TempPromoEnv {
        fn close_flag(&self) -> bool {
            self.close
        }

        fn auth_is_authorized(&self) -> bool {
            self.is_authorized
        }

        fn auth_is_bot(&self) -> bool {
            self.is_bot
        }

        fn now(&self) -> f64 {
            self.now
        }

        fn unix_time(&self) -> i32 {
            self.unix_time
        }

        fn get_option_integer(&self, name: &str, default_value: i64) -> i64 {
            self.options_integer
                .get(name)
                .copied()
                .unwrap_or(default_value)
        }

        fn set_option_boolean(&mut self, name: &str, value: bool) {
            self.options_boolean.insert(name.to_owned(), value);
        }

        fn set_option_empty(&mut self, name: &str) {
            self.emptied_options.push(name.to_owned());
        }

        fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &'static str) {
            self.users_intake.push(source);
            let _ = users;
        }

        fn on_get_chats(&mut self, chats: Vec<telegram_api::Chat>, source: &'static str) {
            self.chats_intake.push(source);
            let _ = chats;
        }

        fn set_sponsored_dialog(&mut self, dialog_id: DialogId, source: DialogSource) {
            self.sponsored_dialog.push((dialog_id, source));
        }

        fn update_suggested_actions(&mut self, actions: &[SuggestedAction]) {
            self.suggested_actions.push(actions.to_vec());
        }

        fn send_get_promo_data_query(&mut self) {
            self.get_promo_queries += 1;
        }

        fn send_hide_promo_data_query(&mut self, dialog_id: DialogId) {
            self.hide_promo_queries.push(dialog_id);
        }
    }

    fn telegram_user(id: i64) -> telegram_api::User {
        telegram_api::User::user(telegram_api::user {
            flags: 0,
            access_hash: 123,
            id,
            first_name: "Promo".to_owned(),
            ..telegram_api::user::default()
        })
    }

    #[test]
    fn init_is_gated_by_auth_bot_and_close() {
        let mut env = TempPromoEnv::new();
        env.is_authorized = false;
        let mut manager = PromoDataManager::new();
        manager.init(&mut env);
        assert!(manager.next_deadline().is_none());

        env.is_authorized = true;
        env.is_bot = true;
        manager.init(&mut env);
        assert!(manager.next_deadline().is_none());

        env.is_bot = false;
        env.close = true;
        manager.init(&mut env);
        assert!(manager.next_deadline().is_none());

        env.close = false;
        manager.init(&mut env);
        // reload_promo_data -> schedule_get_promo_data(0): fires immediately
        assert_eq!(manager.next_deadline(), Some(1000.0));
        manager.tick_timeouts(&mut env);
        assert_eq!(env.get_promo_queries, 1);

        // the init is idempotent
        let queries_before = env.get_promo_queries;
        manager.init(&mut env);
        assert_eq!(env.get_promo_queries, queries_before);
    }

    #[test]
    fn reload_coalesces_behind_the_running_fetch() {
        let mut env = TempPromoEnv::new();
        let mut manager = PromoDataManager::new();
        manager.init(&mut env);
        manager.tick_timeouts(&mut env);
        assert_eq!(env.get_promo_queries, 1);
        assert!(manager.reloading_promo_data);

        // a reload while the fetch is in flight only flips the flag
        manager.reload_promo_data(&mut env);

        let answer =
            telegram_api::help_PromoData::help_promoDataEmpty(telegram_api::help_promoDataEmpty {
                expires: 3600,
            });
        manager.on_get_promo_data(&mut env, Result::Ok(answer), false);
        assert!(!manager.reloading_promo_data);
        // need_reload_promo_data forced expires_at to 0 -> immediate re-fetch
        assert_eq!(manager.next_deadline(), Some(1000.0));
        manager.tick_timeouts(&mut env);
        assert_eq!(env.get_promo_queries, 2);
    }

    #[test]
    fn the_reschedule_clamp_table_is_pinned() {
        let mut env = TempPromoEnv::new();
        let mut manager = PromoDataManager::new();
        manager.init(&mut env);

        // the re-schedule tail: expires_at == 0 -> 0 (immediate)
        manager.on_get_promo_data(
            &mut env,
            Result::Ok(telegram_api::help_PromoData::help_promoDataEmpty(
                telegram_api::help_promoDataEmpty { expires: 0 },
            )),
            false,
        );
        assert_eq!(manager.next_deadline(), Some(1000.0));

        // expires_at in (0, unix_time] -> <= 0 -> 0 (immediate)
        manager.on_get_promo_data(
            &mut env,
            Result::Ok(telegram_api::help_PromoData::help_promoDataEmpty(
                telegram_api::help_promoDataEmpty { expires: 1000 },
            )),
            false,
        );
        assert_eq!(manager.next_deadline(), Some(1000.0));

        // expires_at - unix_time in (0, 60) -> clamped to 60
        manager.on_get_promo_data(
            &mut env,
            Result::Ok(telegram_api::help_PromoData::help_promoDataEmpty(
                telegram_api::help_promoDataEmpty { expires: 1030 },
            )),
            false,
        );
        assert_eq!(manager.next_deadline(), Some(1060.0));

        // in-range expires survive as the exact difference
        manager.on_get_promo_data(
            &mut env,
            Result::Ok(telegram_api::help_PromoData::help_promoDataEmpty(
                telegram_api::help_promoDataEmpty { expires: 1250 },
            )),
            false,
        );
        assert_eq!(manager.next_deadline(), Some(1250.0));

        // above the cap -> clamped to 86400
        manager.on_get_promo_data(
            &mut env,
            Result::Ok(telegram_api::help_PromoData::help_promoDataEmpty(
                telegram_api::help_promoDataEmpty { expires: 1000000 },
            )),
            false,
        );
        assert_eq!(manager.next_deadline(), Some(1000.0 + 86400.0));
    }

    #[test]
    fn an_error_reschedules_in_sixty_seconds() {
        let mut env = TempPromoEnv::new();
        let mut manager = PromoDataManager::new();
        manager.init(&mut env);
        manager.tick_timeouts(&mut env);
        manager.on_get_promo_data(
            &mut env,
            Result::Err(tdutils::status::Status::error(400, "FLOOD_WAIT_5")),
            false,
        );
        assert!(!manager.reloading_promo_data);
        assert_eq!(manager.next_deadline(), Some(1060.0));
    }

    #[test]
    fn the_empty_promo_removes_the_sponsored_dialog() {
        let mut env = TempPromoEnv::new();
        let mut manager = PromoDataManager::new();
        manager.init(&mut env);
        manager.on_get_promo_data(
            &mut env,
            Result::Ok(telegram_api::help_PromoData::help_promoDataEmpty(
                telegram_api::help_promoDataEmpty { expires: 100 },
            )),
            false,
        );
        // the C++ empty-default DialogId/DialogSource pair
        assert_eq!(
            env.sponsored_dialog,
            vec![(DialogId::default(), DialogSource::Membership)]
        );
    }

    #[test]
    fn the_full_promo_intake_halves_are_pinned() {
        let mut env = TempPromoEnv::new();
        let mut manager = PromoDataManager::new();
        manager.init(&mut env);

        let peer = telegram_api::Peer::peerUser(telegram_api::peerUser { user_id: 777000 });
        let mut promo = telegram_api::help_promoData {
            flags: 0,
            proxy: false,
            expires: 2000,
            peer: Some(peer),
            psa_type: "psa_type".to_owned(),
            psa_message: "psa message".to_owned(),
            pending_suggestions: vec![
                "SETUP_PASSWORD".to_owned(),
                "NEWCOMER_TICKS".to_owned(),
                "NOT_A_REAL_ACTION".to_owned(),
            ],
            dismissed_suggestions: vec!["BIRTHDAY_CONTACTS_TODAY".to_owned()],
            custom_pending_suggestion: Some(telegram_api::PendingSuggestion {
                suggestion: "custom hint".to_owned(),
                title: None,
                description: None,
                url: "https://example.com".to_owned(),
            }),
            chats: vec![telegram_api::Chat::chatEmpty(telegram_api::chatEmpty {
                id: -3,
            })],
            users: vec![telegram_user(777000)],
        };
        manager.on_get_promo_data(
            &mut env,
            Result::Ok(telegram_api::help_PromoData::help_promoData(promo.clone())),
            false,
        );

        // the users/chats intakes
        assert_eq!(env.users_intake, vec!["on_get_promo_data"]);
        assert_eq!(env.chats_intake, vec!["on_get_promo_data"]);

        // the psa source from the non-proxy peer
        assert_eq!(
            env.sponsored_dialog,
            vec![(
                DialogId::new(777000),
                DialogSource::PublicServiceAnnouncement {
                    psa_type: "psa_type".to_owned(),
                    psa_text: "psa message".to_owned(),
                }
            )]
        );

        // the dismissed birthday suggestion wins the option boolean
        assert!(env.options_boolean["dismiss_birthday_contact_today"]);

        // the suggestion table: SETUP_PASSWORD lands while no relogin
        // deadline is set, NEWCOMER_TICKS lands, unknown drops, and the
        // custom suggestion appends
        let actions = &env.suggested_actions[0];
        assert_eq!(
            actions,
            &vec![
                SuggestedAction::from_action_str("SETUP_PASSWORD"),
                SuggestedAction::from_action_str("NEWCOMER_TICKS"),
                SuggestedAction::from_pending_suggestion(
                    promo.custom_pending_suggestion.as_ref().unwrap()
                ),
            ]
        );

        // the proxy peer maps to the mtproto-proxy source
        promo.proxy = true;
        let mut env = TempPromoEnv::new();
        manager.on_get_promo_data(
            &mut env,
            Result::Ok(telegram_api::help_PromoData::help_promoData(promo)),
            false,
        );
        assert_eq!(env.sponsored_dialog[0].1, DialogSource::MtprotoProxy);
    }

    #[test]
    fn the_set_password_suggestion_skips_behind_relogin_days() {
        let build = |relogin_days: i64| {
            let mut env = TempPromoEnv::new();
            if relogin_days != 0 {
                env.options_integer
                    .insert("otherwise_relogin_days".to_owned(), relogin_days);
            }
            let mut manager = PromoDataManager::new();
            manager.init(&mut env);
            let peer = telegram_api::Peer::peerUser(telegram_api::peerUser { user_id: 1 });
            let promo = telegram_api::help_promoData {
                flags: 0,
                proxy: false,
                expires: 0,
                peer: Some(peer),
                psa_type: String::new(),
                psa_message: String::new(),
                pending_suggestions: vec!["SETUP_PASSWORD".to_owned()],
                dismissed_suggestions: Vec::new(),
                custom_pending_suggestion: None,
                chats: Vec::new(),
                users: vec![telegram_user(1)],
            };
            manager.on_get_promo_data(
                &mut env,
                Result::Ok(telegram_api::help_PromoData::help_promoData(promo)),
                false,
            );
            (env, manager)
        };

        // with no relogin deadline the suggestion lands
        let (env, _) = build(0);
        assert_eq!(
            env.suggested_actions[0],
            vec![SuggestedAction::from_action_str("SETUP_PASSWORD")]
        );

        // behind a positive otherwise_relogin_days it is skipped
        let (env, _) = build(7);
        assert!(env.suggested_actions[0].is_empty());
    }

    #[test]
    fn the_custom_suggestion_gate_needs_both_strings() {
        let build = |suggestion: &str, url: &str| {
            let mut env = TempPromoEnv::new();
            let mut manager = PromoDataManager::new();
            manager.init(&mut env);
            let peer = telegram_api::Peer::peerUser(telegram_api::peerUser { user_id: 1 });
            let promo = telegram_api::help_promoData {
                flags: 0,
                proxy: false,
                expires: 0,
                peer: Some(peer),
                psa_type: String::new(),
                psa_message: String::new(),
                pending_suggestions: Vec::new(),
                dismissed_suggestions: Vec::new(),
                custom_pending_suggestion: Some(telegram_api::PendingSuggestion {
                    suggestion: suggestion.to_owned(),
                    title: None,
                    description: None,
                    url: url.to_owned(),
                }),
                chats: Vec::new(),
                users: Vec::new(),
            };
            manager.on_get_promo_data(
                &mut env,
                Result::Ok(telegram_api::help_PromoData::help_promoData(promo)),
                false,
            );
            env
        };

        // both strings present -> the custom action lands
        let env = build("hint", "https://example.com");
        assert_eq!(env.suggested_actions[0].len(), 1);
        assert_eq!(
            env.suggested_actions[0][0].get_action_type(),
            SuggestedActionType::Custom
        );

        // an empty url (or suggestion) drops the action, like the C++ gate
        let env = build("hint", "");
        assert!(env.suggested_actions[0].is_empty());
        let env = build("", "https://example.com");
        assert!(env.suggested_actions[0].is_empty());
    }

    #[test]
    fn hide_promo_data_removes_then_sends_the_tail() {
        let mut env = TempPromoEnv::new();
        let mut manager = PromoDataManager::new();
        manager.init(&mut env);
        let dialog_id = DialogId::new(-1000000000000123);
        manager.hide_promo_data(&mut env, dialog_id);
        // the local half removed the sponsored dialog before the query
        assert_eq!(
            env.sponsored_dialog,
            vec![(DialogId::default(), DialogSource::Membership)]
        );
        assert_eq!(env.hide_promo_queries, vec![dialog_id]);
    }

    #[test]
    fn the_close_flag_silences_the_intake_and_the_tick() {
        let mut env = TempPromoEnv::new();
        let mut manager = PromoDataManager::new();
        manager.init(&mut env);
        manager.tick_timeouts(&mut env);

        env.close = true;
        manager.on_get_promo_data(
            &mut env,
            Result::Ok(telegram_api::help_PromoData::help_promoDataEmpty(
                telegram_api::help_promoDataEmpty { expires: 100 },
            )),
            false,
        );
        assert!(env.sponsored_dialog.is_empty());
        // reloading stays set: the C++ returns before the reset
        assert!(manager.reloading_promo_data);

        manager.tick_timeouts(&mut env);
        assert_eq!(env.get_promo_queries, 1);
    }
}
