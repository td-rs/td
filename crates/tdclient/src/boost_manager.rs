//! `BoostManager` (td/telegram/BoostManager.{h,cpp}) — the M.38 Wave-3
//! chat-boost manager: the option-driven boost feature tables and the
//! boost-link builders (the value plane) plus the router over the query
//! seams. Recorded collapses: the theme counts ride the ThemeManager
//! (M.41, not started) — the seam answers zero; the five query handlers
//! ride the `BoostNetKind` plane (the net-drain 59 families) over the
//! manager-held `crate::td::BoostNetHandle` — the answers drain through
//! `Td::on_boost_net_answer`; the link parse
//! (`LinkManager::get_dialog_boost_link_info`) and the username/dialog
//! resolution are the M.23/M.6 seams.

use crate::chat_manager::ChannelId;
use crate::dialog_id::DialogId;
use crate::td_api;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The boost-plane in-flight query kinds — the C++ `Td::ResultHandler`
/// classes of BoostManager.cpp:94-325 collapsed onto dispatcher tags; the
/// answers drain through [`crate::td::Td::on_boost_net_answer`]. The kinds
/// carry the handlers' `dialog_id_` members (the `on_error` dialog head);
/// the request promises park in the manager's family lots (the C++ carries
/// them on the queries' `promise_` members — §5's FIFO-per-family shape).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoostNetKind {
    /// `GetMyBoostsQuery` (BoostManager.cpp:94-118) — `premium.getMyBoosts`;
    /// the answer converts through
    /// [`BoostManager::get_chat_boost_slots_object`] (cpp:47-93).
    GetMyBoosts,
    /// `ApplyBoostQuery` (BoostManager.cpp:228-261) —
    /// `premium.applyBoost`; the kind carries the handler's `dialog_id_`
    /// member for the `on_error` dialog head (cpp:260); the answer is
    /// the shared `premium.myBoosts` conversion of the slots family.
    ApplyBoost { dialog_id: DialogId },
    /// `GetBoostsStatusQuery` (BoostManager.cpp:138-225) —
    /// `premium.getBoostsStatus`; the kind carries the handler's
    /// `dialog_id_` member for the `on_error` dialog head (cpp:193) and
    /// the channel-audience band (cpp:161-163); the answer converts at
    /// [`BoostManager::get_chat_boost_status_object`].
    GetBoostsStatus { dialog_id: DialogId },
}

/// `td::DialogBoostLinkInfo` (DialogBoostLinkInfo.h).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DialogBoostLinkInfo {
    pub username: String,
    pub channel_id: ChannelId,
}

/// The `G()`/`td_` surface of the manager.
pub trait BoostManagerEnv {
    fn close_flag(&self) -> bool;

    fn get_option_integer(&self, name: &str, default_value: i64) -> i64;

    fn send_update(&mut self, update: td_api::Update);

    /// `DialogManager::check_dialog_access(dialog_id, false, Read,
    /// source)` — the local gate of the boost families.
    fn check_dialog_access(
        &mut self,
        dialog_id: DialogId,
        source: &str,
    ) -> std::result::Result<(), Status>;

    /// `ChatManager::get_channel_first_username` — the link public half.
    fn get_channel_first_username(&mut self, channel_id: ChannelId) -> String;

    /// `ThemeManager::get_dialog_boost_available_count` — the M.41 seam
    /// (zero counts until the themes track lands).
    fn get_dialog_boost_available_count(
        &mut self,
        _level: i32,
        _for_megagroup: bool,
    ) -> (i32, i32, i32) {
        (0, 0, 0)
    }

    // ---- the answer-conversion reads (the net-drain 59 families) ----

    /// `G()->unix_time()` — the slot/boost expiry filters.
    fn unix_time(&mut self) -> i32 {
        0
    }

    /// `td_->user_manager_->on_get_users(users, source)` — the answer
    /// intakes.
    fn on_get_users(&mut self, _users: Vec<telegram_api::User>, _source: &str) {}

    /// `td_->chat_manager_->on_get_chats(chats, source)`.
    fn on_get_chats(&mut self, _chats: Vec<telegram_api::Chat>, _source: &str) {}

    /// `td_->dialog_manager_->force_create_dialog(dialog_id, source,
    /// force_read)` (DialogManager.cpp:1540).
    fn force_create_dialog(&mut self, _dialog_id: DialogId, _source: &str, _force_read: bool) {}

    /// `td_->dialog_manager_->get_chat_id_object(dialog_id, source)`
    /// (DialogManager.cpp:1690).
    fn get_chat_id_object(&mut self, dialog_id: DialogId, _source: &str) -> i64 {
        dialog_id.get()
    }

    /// `td_->dialog_manager_->get_input_peer(dialog_id, access_rights)`
    /// (DialogManager.cpp:1377) — the query send legs resolve the peer.
    fn get_input_peer(
        &mut self,
        dialog_id: DialogId,
        access_rights: crate::user_manager::AccessRights,
    ) -> Option<telegram_api::InputPeer>;

    /// `td_->dialog_manager_->on_get_dialog_error(dialog_id, status,
    /// source)` (DialogManager.cpp:1996) — the ResultHandler `on_error`
    /// intake.
    fn on_get_dialog_error(&mut self, dialog_id: DialogId, status: &Status, source: &str);

    /// `td_->chat_manager_->on_update_channel_participant_count(
    /// channel_id, count)` (ChatManager.cpp:7994) — the channel band of
    /// the status answer's premium audience; the M.5 participant-count
    /// plane stand-in.
    fn on_update_channel_participant_count(&mut self, _channel_id: ChannelId, _count: i32) {}

    fn send_get_boosts_list_query(
        &mut self,
        dialog_id: DialogId,
        only_gift_codes: bool,
        offset: &str,
        limit: i32,
    ) {
        tdutils::log_info!("Send premium.getBoostsList for {dialog_id:?} — [blocked: net]");
        let _ = (only_gift_codes, offset, limit);
    }
    fn send_get_user_boosts_query(&mut self, dialog_id: DialogId, user_id: i64) {
        tdutils::log_info!("Send premium.getUserBoosts for {dialog_id:?} — [blocked: net]");
        let _ = user_id;
    }
}

/// The collapsed `BoostManager`.
#[derive(Default)]
pub struct BoostManager {
    /// The manager-held dispatch surface of the net-drain 59 families —
    /// `None` keeps the net-inert posture (the pre-init windows and the
    /// net-inert tests; the `GameManager::net` shape).
    pub(crate) net: Option<crate::td::BoostNetHandle>,
    /// The `GetMyBoostsQuery` promises (the handler's `promise_` member,
    /// cpp:96) — popped FIFO by the `premium.myBoosts` answers of the
    /// slots family.
    pub(crate) pending_get_my_boosts: Vec<Promise<td_api::ChatBoostSlots>>,
    /// The `ApplyBoostQuery` promises (the handler's `promise_` member,
    /// cpp:230) — popped FIFO by the apply family's
    /// `premium.myBoosts` answers.
    pub(crate) pending_apply_boost: Vec<Promise<td_api::ChatBoostSlots>>,
    /// The `GetBoostsStatusQuery` promises (the handler's `promise_`
    /// member, cpp:123) — popped FIFO by the `premium.boostsStatus`
    /// answers.
    pub(crate) pending_get_boosts_status: Vec<Promise<td_api::ChatBoostStatus>>,
    /// The `FoundChatBoosts`-typed promises parked at the list seams.
    pub parked_found_boosts: Vec<Promise<td_api::FoundChatBoosts>>,
}

impl BoostManager {
    pub fn new() -> BoostManager {
        BoostManager::default()
    }

    /// `get_chat_boost_level_features_object` (BoostManager.cpp:365-390) —
    /// the theme counts are the M.41 seam.
    pub fn get_chat_boost_level_features_object(
        &mut self,
        env: &mut impl BoostManagerEnv,
        for_megagroup: bool,
        level: i32,
    ) -> td_api::ChatBoostLevelFeatures {
        let max_level = env.get_option_integer("chat_boost_level_max", 10) as i32;
        let actual_level = level.clamp(0, max_level);
        let key_prefix = if for_megagroup { "group" } else { "channel" };
        let have_enough_boost_level = |name: &str| {
            let needed_boost_level =
                env.get_option_integer(&format!("{key_prefix}_{name}_level_min"), 0) as i32;
            needed_boost_level != 0 && actual_level >= needed_boost_level
        };
        let can_set_profile_background_custom_emoji = have_enough_boost_level("profile_bg_icon");
        let can_set_background_custom_emoji = have_enough_boost_level("bg_icon");
        let can_set_emoji_status = have_enough_boost_level("emoji_status");
        let can_set_custom_background = have_enough_boost_level("custom_wallpaper");
        let can_set_custom_emoji_sticker_set = have_enough_boost_level("emoji_stickers");
        let can_enable_automatic_translation = have_enough_boost_level("autotranslation");
        let can_recognize_speech = have_enough_boost_level("transcribe");
        let can_disable_sponsored_messages = have_enough_boost_level("restrict_sponsored");
        let theme_counts = env.get_dialog_boost_available_count(actual_level, for_megagroup);
        td_api::ChatBoostLevelFeatures {
            level,
            story_per_day_count: if for_megagroup { 0 } else { actual_level },
            custom_emoji_reaction_count: actual_level,
            title_color_count: theme_counts.0,
            profile_accent_color_count: theme_counts.1,
            can_set_profile_background_custom_emoji,
            accent_color_count: theme_counts.2,
            can_set_background_custom_emoji,
            can_set_emoji_status,
            chat_theme_background_count: 0,
            can_set_custom_background,
            can_set_custom_emoji_sticker_set,
            can_enable_automatic_translation,
            can_recognize_speech,
            can_disable_sponsored_messages,
        }
    }

    /// `get_chat_boost_features_object` (BoostManager.cpp:391-414).
    pub fn get_chat_boost_features_object(
        &mut self,
        env: &mut impl BoostManagerEnv,
        for_megagroup: bool,
    ) -> td_api::ChatBoostFeatures {
        let key_prefix = if for_megagroup { "group" } else { "channel" };
        let min = |name: &str| {
            env.get_option_integer(&format!("{key_prefix}_{name}_level_min"), 1_000_000_000) as i32
        };
        let min_profile_background_custom_emoji_boost_level = min("profile_bg_icon");
        let min_background_custom_emoji_boost_level = min("bg_icon");
        let min_emoji_status_boost_level = min("emoji_status");
        let min_chat_theme_background_boost_level = min("wallpaper");
        let min_custom_background_boost_level = min("custom_wallpaper");
        let min_custom_emoji_sticker_set_boost_level = min("emoji_stickers");
        let min_automatic_translation_boost_level = min("autotranslation");
        let min_speech_recognition_boost_level = min("transcribe");
        let min_sponsored_message_disable_boost_level = min("restrict_sponsored");
        let mut features = Vec::new();
        let max_level = env.get_option_integer("chat_boost_level_max", 10) as i32;
        for level in 1..=max_level {
            features.push(self.get_chat_boost_level_features_object(env, for_megagroup, level));
        }
        td_api::ChatBoostFeatures {
            features,
            min_profile_background_custom_emoji_boost_level,
            min_background_custom_emoji_boost_level,
            min_emoji_status_boost_level,
            min_chat_theme_background_boost_level,
            min_custom_background_boost_level,
            min_custom_emoji_sticker_set_boost_level,
            min_automatic_translation_boost_level,
            min_speech_recognition_boost_level,
            min_sponsored_message_disable_boost_level,
        }
    }

    /// `get_boost_slots` (cpp:416-419) — the handler collapse parks the
    /// request promise in the family lot and dispatches the wire query.
    pub fn get_boost_slots(
        &mut self,
        _env: &mut impl BoostManagerEnv,
        promise: Promise<td_api::ChatBoostSlots>,
    ) {
        self.send_get_my_boosts_query(promise);
    }

    /// The `td_->create_handler<GetMyBoostsQuery>(promise)->send()`
    /// collapse (BoostManager.cpp:94-118) — the request promise parks in
    /// the family lot (the C++ carries it on the query's `promise_`
    /// member, cpp:96 — §5's FIFO shape), then the wire query dispatches;
    /// `None` on the handle keeps the net-inert posture.
    pub(crate) fn send_get_my_boosts_query(&mut self, promise: Promise<td_api::ChatBoostSlots>) {
        self.pending_get_my_boosts.push(promise);
        match self.net.clone() {
            // `GetMyBoostsQuery::send` (cpp:100)
            Some(net) => net.dispatch(
                BoostNetKind::GetMyBoosts,
                telegram_api::Function::premium_getMyBoosts(telegram_api::premium_getMyBoosts),
            ),
            None => tdutils::log_info!("Send premium.getMyBoosts — [net-inert]"),
        }
    }

    /// The FIFO-head promise pop for the `Td::on_boost_net_answer` drain
    /// (the C++ promise rides the query).
    pub(crate) fn pop_get_my_boosts_promise(&mut self) -> Option<Promise<td_api::ChatBoostSlots>> {
        (!self.pending_get_my_boosts.is_empty()).then(|| self.pending_get_my_boosts.remove(0))
    }

    /// `get_chat_boost_slots_object` (BoostManager.cpp:47-93) — the
    /// `premium.myBoosts` answer conversion of the slots family (and of
    /// the apply family, cpp:139-143): the users/chats intakes run first,
    /// then the slot loop (the expiry filter, the date clamps, the
    /// invalid-peer drop and the empty-slot date zeroing).
    pub(crate) fn get_chat_boost_slots_object(
        &mut self,
        env: &mut impl BoostManagerEnv,
        my_boosts: telegram_api::premium_MyBoosts,
    ) -> td_api::ChatBoostSlots {
        env.on_get_users(my_boosts.users, "GetMyBoostsQuery");
        env.on_get_chats(my_boosts.chats, "GetMyBoostsQuery");
        let mut slots = Vec::new();
        for my_boost in my_boosts.my_boosts {
            let expiration_date = my_boost.expires;
            if expiration_date <= env.unix_time() {
                continue;
            }

            let mut start_date = my_boost.date.max(0);
            let mut cooldown_until_date = my_boost.cooldown_until_date.max(0);
            let mut dialog_id = DialogId::default();
            if let Some(peer) = my_boost.peer.as_ref() {
                dialog_id = DialogId::from_peer(peer);
                if !dialog_id.is_valid() {
                    tdutils::log_error!("Receive invalid {dialog_id:?} in the boost slots");
                    continue;
                }
            }
            if dialog_id.is_valid() {
                env.force_create_dialog(dialog_id, "GetMyBoostsQuery", true);
            } else {
                start_date = 0;
                cooldown_until_date = 0;
            }
            slots.push(td_api::ChatBoostSlot {
                slot_id: my_boost.slot,
                currently_boosted_chat_id: env.get_chat_id_object(dialog_id, "GetMyBoostsQuery"),
                start_date,
                expiration_date,
                cooldown_until_date,
            });
        }
        td_api::ChatBoostSlots { slots }
    }

    /// `get_dialog_boost_status` (cpp:420-426) — the dialog-access gate
    /// answers its band on the promise, then the handler collapse parks
    /// the promise in the family lot and dispatches the wire query.
    pub fn get_dialog_boost_status(
        &mut self,
        env: &mut impl BoostManagerEnv,
        dialog_id: DialogId,
        mut promise: Promise<td_api::ChatBoostStatus>,
    ) {
        if let Err(status) = env.check_dialog_access(dialog_id, "get_dialog_boost_status") {
            return promise.set_error(status);
        }
        self.send_get_boosts_status_query(env, dialog_id, promise);
    }

    /// The `td_->create_handler<GetBoostsStatusQuery>(promise)
    /// ->send(dialog_id)` collapse (BoostManager.cpp:120-133): the
    /// request promise parks in the family lot (the C++ carries it on the
    /// query's `promise_` member, cpp:123 — §5's FIFO shape), then the
    /// read peer resolves and the wire query dispatches. The C++
    /// `CHECK(input_peer != nullptr)` (cpp:131) is the port's
    /// `complete_error` file-through (the message-query CHECK convention,
    /// MessageQueryManager.cpp:1182-1193): the 400 runs the full
    /// `on_error` tail. `None` on the handle keeps the net-inert posture.
    pub(crate) fn send_get_boosts_status_query(
        &mut self,
        env: &mut impl BoostManagerEnv,
        dialog_id: DialogId,
        promise: Promise<td_api::ChatBoostStatus>,
    ) {
        self.pending_get_boosts_status.push(promise);
        let kind = BoostNetKind::GetBoostsStatus { dialog_id };
        match (
            self.net.clone(),
            env.get_input_peer(dialog_id, crate::user_manager::AccessRights::Read),
        ) {
            // `GetBoostsStatusQuery::send` (cpp:128-133)
            (Some(net), Some(peer)) => net.dispatch(
                kind,
                telegram_api::Function::premium_getBoostsStatus(
                    telegram_api::premium_getBoostsStatus {
                        peer: Some(Box::new(peer)),
                    },
                ),
            ),
            (Some(net), None) => net.complete_error(kind, 400, "Can't access the chat"),
            (None, _) => {
                tdutils::log_info!("Send premium.getBoostsStatus for {dialog_id:?} — [net-inert]")
            }
        }
    }

    /// The FIFO-head promise pop for the `Td::on_boost_net_answer` drain
    /// (the C++ promise rides the query).
    pub(crate) fn pop_get_boosts_status_promise(
        &mut self,
    ) -> Option<Promise<td_api::ChatBoostStatus>> {
        (!self.pending_get_boosts_status.is_empty())
            .then(|| self.pending_get_boosts_status.remove(0))
    }

    /// The `premium.boostsStatus` answer conversion
    /// (BoostManager.cpp:135-191) — the invalid-answer validation ladder
    /// (the negative level, the boosts below the level floor and the
    /// next-level ceiling), the premium-audience counts (the channel
    /// participant-count band and the percentage), the prepaid-giveaway
    /// transform and the boost/gift-count clamps.
    pub(crate) fn get_chat_boost_status_object(
        &mut self,
        env: &mut impl BoostManagerEnv,
        dialog_id: DialogId,
        mut result: telegram_api::premium_BoostsStatus,
    ) -> td_api::ChatBoostStatus {
        if result.level < 0
            || result.current_level_boosts < 0
            || result.boosts < result.current_level_boosts
            || (result.next_level_boosts != 0 && result.boosts >= result.next_level_boosts)
        {
            tdutils::log_error!("Receive invalid premium.boostsStatus answer");
            if result.level < 0 {
                result.level = 0;
            }
            if result.current_level_boosts < 0 {
                result.current_level_boosts = 0;
            }
            if result.boosts < result.current_level_boosts {
                result.boosts = result.current_level_boosts;
            }
            if result.next_level_boosts != 0 && result.boosts >= result.next_level_boosts {
                result.next_level_boosts = result.boosts + 1;
            }
        }
        let mut premium_member_count = 0;
        let mut premium_member_percentage = 0.0;
        if let Some(audience) = result.premium_audience.as_ref() {
            premium_member_count = (audience.part as i32).max(0);
            let participant_count = (audience.total as i32).max(premium_member_count);
            if dialog_id.get_type() == crate::dialog_id::DialogType::Channel {
                env.on_update_channel_participant_count(
                    dialog_id.get_channel_id(),
                    participant_count,
                );
            }
            if participant_count > 0 {
                premium_member_percentage =
                    100.0 * premium_member_count as f64 / participant_count as f64;
            }
        }
        let giveaways = result
            .prepaid_giveaways
            .into_iter()
            .map(|giveaway| match giveaway {
                telegram_api::PrepaidGiveaway::prepaidGiveaway(giveaway) => {
                    td_api::PrepaidGiveaway {
                        id: giveaway.id,
                        winner_count: giveaway.quantity,
                        prize: Some(td_api::GiveawayPrize::giveawayPrizePremium(
                            td_api::giveawayPrizePremium {
                                month_count: giveaway.months,
                            },
                        )),
                        boost_count: 0,
                        payment_date: giveaway.date,
                    }
                }
                telegram_api::PrepaidGiveaway::prepaidStarsGiveaway(giveaway) => {
                    td_api::PrepaidGiveaway {
                        id: giveaway.id,
                        winner_count: giveaway.quantity,
                        prize: Some(td_api::GiveawayPrize::giveawayPrizeStars(
                            td_api::giveawayPrizeStars {
                                star_count: crate::star_manager::get_star_count(
                                    giveaway.stars,
                                    false,
                                ),
                            },
                        )),
                        boost_count: giveaway.boosts,
                        payment_date: giveaway.date,
                    }
                }
            })
            .collect();
        let boost_count = result.boosts.max(0);
        let gift_code_boost_count = result.gift_boosts.clamp(0, boost_count);
        td_api::ChatBoostStatus {
            boost_url: result.boost_url,
            applied_slot_ids: result.my_boost_slots,
            level: result.level,
            gift_code_boost_count,
            boost_count,
            current_level_boost_count: result.current_level_boosts,
            next_level_boost_count: result.next_level_boosts,
            premium_member_count,
            premium_member_percentage,
            prepaid_giveaways: giveaways,
        }
    }

    /// `boost_dialog` (cpp:427-433) — the empty-slot list answers the
    /// slots query.
    pub fn boost_dialog(
        &mut self,
        env: &mut impl BoostManagerEnv,
        dialog_id: DialogId,
        slot_ids: &[i32],
        mut promise: Promise<td_api::ChatBoostSlots>,
    ) {
        if let Err(status) = env.check_dialog_access(dialog_id, "boost_dialog") {
            return promise.set_error(status);
        }
        if slot_ids.is_empty() {
            return self.get_boost_slots(env, promise);
        }
        self.send_apply_boost_query(env, dialog_id, slot_ids, promise);
    }

    /// The `td_->create_handler<ApplyBoostQuery>(promise)
    /// ->send(dialog_id, slot_ids)` collapse (BoostManager.cpp:237-247):
    /// the request promise parks in the family lot (the C++ carries it on
    /// the query's `promise_` member, cpp:230 — §5's FIFO shape), then the
    /// read peer resolves and the wire query dispatches with the
    /// SLOTS_MASK flag. The C++ `CHECK(input_peer != nullptr)` (cpp:241)
    /// is the port's `complete_error` file-through (the message-query
    /// CHECK convention). `None` on the handle keeps the net-inert
    /// posture.
    pub(crate) fn send_apply_boost_query(
        &mut self,
        env: &mut impl BoostManagerEnv,
        dialog_id: DialogId,
        slot_ids: &[i32],
        promise: Promise<td_api::ChatBoostSlots>,
    ) {
        self.pending_apply_boost.push(promise);
        let kind = BoostNetKind::ApplyBoost { dialog_id };
        match (
            self.net.clone(),
            env.get_input_peer(dialog_id, crate::user_manager::AccessRights::Read),
        ) {
            // `ApplyBoostQuery::send` (cpp:237-247)
            (Some(net), Some(peer)) => net.dispatch(
                kind,
                telegram_api::Function::premium_applyBoost(telegram_api::premium_applyBoost {
                    flags: 1, // SLOTS_MASK
                    slots: slot_ids.to_vec(),
                    peer: Some(Box::new(peer)),
                }),
            ),
            (Some(net), None) => net.complete_error(kind, 400, "Can't access the chat"),
            (None, _) => {
                tdutils::log_info!("Send premium.applyBoost for {dialog_id:?} — [net-inert]")
            }
        }
    }

    /// The FIFO-head promise pop for the `Td::on_boost_net_answer` drain
    /// (the C++ promise rides the query).
    pub(crate) fn pop_apply_boost_promise(&mut self) -> Option<Promise<td_api::ChatBoostSlots>> {
        (!self.pending_apply_boost.is_empty()).then(|| self.pending_apply_boost.remove(0))
    }

    /// `get_dialog_boost_link` (cpp:434-457) — the local link build (the
    /// `get_t_me_url` default; the option gate joins M.23).
    pub fn get_dialog_boost_link(
        &mut self,
        env: &mut impl BoostManagerEnv,
        dialog_id: DialogId,
    ) -> std::result::Result<(String, bool), Status> {
        env.check_dialog_access(dialog_id, "get_dialog_boost_link")?;
        if dialog_id.get_type() != crate::dialog_id::DialogType::Channel {
            return Err(Status::error(400, "Can't boost the chat"));
        }
        let channel_id = dialog_id.get_channel_id();
        let username = env.get_channel_first_username(channel_id);
        let is_public = !username.is_empty();
        let link = if is_public {
            format!("https://t.me/boost/{username}")
        } else {
            format!("https://t.me/boost?c={}", channel_id.get())
        };
        Ok((link, is_public))
    }

    /// `get_chat_boost_link_info_object` (cpp:470-479) — the dialog
    /// resolution is the M.6 seam (the chat id answers the raw id).
    pub fn get_chat_boost_link_info_object(
        &self,
        info: &DialogBoostLinkInfo,
    ) -> td_api::ChatBoostLinkInfo {
        let is_public = !info.username.is_empty();
        let dialog_id = if is_public {
            DialogId::default()
        } else {
            DialogId::from_channel_id(info.channel_id)
        };
        td_api::ChatBoostLinkInfo {
            is_public,
            chat_id: dialog_id.get(),
        }
    }

    /// `get_dialog_boosts` (cpp:481-490).
    pub fn get_dialog_boosts(
        &mut self,
        env: &mut impl BoostManagerEnv,
        dialog_id: DialogId,
        only_gift_codes: bool,
        offset: &str,
        limit: i32,
        promise: Promise<td_api::FoundChatBoosts>,
    ) {
        env.send_get_boosts_list_query(dialog_id, only_gift_codes, offset, limit);
        self.parked_found_boosts.push(promise);
    }

    /// `get_user_dialog_boosts` (cpp:492-502).
    pub fn get_user_dialog_boosts(
        &mut self,
        env: &mut impl BoostManagerEnv,
        dialog_id: DialogId,
        user_id: i64,
        promise: Promise<td_api::FoundChatBoosts>,
    ) {
        env.send_get_user_boosts_query(dialog_id, user_id);
        self.parked_found_boosts.push(promise);
    }

    /// `on_update_dialog_boost` (cpp:503-521) — the boost intake; the
    /// C++ emits updateChatBoost via the update builders (recorded seam —
    /// the update joins the list-answer converters).
    pub fn on_update_dialog_boost(&mut self, env: &mut impl BoostManagerEnv, dialog_id: DialogId) {
        let _ = (env, dialog_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct TestEnv {
        options: BTreeMap<String, i64>,
        queries: Vec<String>,
        username: String,
        now: i32,
        intakes: Vec<String>,
        forced_dialogs: Vec<DialogId>,
        resolve_peer: bool,
        dialog_errors: Vec<String>,
        participant_counts: Vec<(i64, i32)>,
    }

    impl BoostManagerEnv for TestEnv {
        fn close_flag(&self) -> bool {
            false
        }

        fn get_option_integer(&self, name: &str, default_value: i64) -> i64 {
            self.options.get(name).copied().unwrap_or(default_value)
        }

        fn send_update(&mut self, _update: td_api::Update) {}

        fn check_dialog_access(
            &mut self,
            dialog_id: DialogId,
            _source: &str,
        ) -> std::result::Result<(), Status> {
            if dialog_id.is_valid() {
                Ok(())
            } else {
                Err(Status::error(400, "Chat not found"))
            }
        }

        fn get_channel_first_username(&mut self, _channel_id: ChannelId) -> String {
            self.username.clone()
        }

        fn send_get_boosts_list_query(
            &mut self,
            dialog_id: DialogId,
            only_gift_codes: bool,
            offset: &str,
            limit: i32,
        ) {
            self.queries.push(format!(
                "getBoostsList {} {only_gift_codes} {offset} {limit}",
                dialog_id.get()
            ));
        }

        fn send_get_user_boosts_query(&mut self, dialog_id: DialogId, user_id: i64) {
            self.queries
                .push(format!("getUserBoosts {} {user_id}", dialog_id.get()));
        }

        fn unix_time(&mut self) -> i32 {
            self.now
        }

        fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str) {
            self.intakes.push(format!("users {} {source}", users.len()));
        }

        fn on_get_chats(&mut self, chats: Vec<telegram_api::Chat>, source: &str) {
            self.intakes.push(format!("chats {} {source}", chats.len()));
        }

        fn force_create_dialog(&mut self, dialog_id: DialogId, source: &str, _force_read: bool) {
            self.intakes.push(format!("force {source}"));
            self.forced_dialogs.push(dialog_id);
        }

        fn get_input_peer(
            &mut self,
            _dialog_id: DialogId,
            _access_rights: crate::user_manager::AccessRights,
        ) -> Option<telegram_api::InputPeer> {
            if self.resolve_peer {
                Some(telegram_api::InputPeer::inputPeerSelf(
                    telegram_api::inputPeerSelf,
                ))
            } else {
                None
            }
        }

        fn on_get_dialog_error(&mut self, dialog_id: DialogId, status: &Status, source: &str) {
            self.dialog_errors
                .push(format!("{source} {} {}", dialog_id.get(), status.message()));
        }

        fn on_update_channel_participant_count(&mut self, channel_id: ChannelId, count: i32) {
            self.participant_counts.push((channel_id.get(), count));
        }
    }

    fn env() -> TestEnv {
        let mut options = BTreeMap::new();
        options.insert("chat_boost_level_max".to_string(), 2);
        options.insert("channel_custom_wallpaper_level_min".to_string(), 1);
        options.insert("channel_transcribe_level_min".to_string(), 2);
        TestEnv {
            options,
            queries: Vec::new(),
            username: String::new(),
            now: 1000,
            intakes: Vec::new(),
            forced_dialogs: Vec::new(),
            resolve_peer: true,
            dialog_errors: Vec::new(),
            participant_counts: Vec::new(),
        }
    }

    #[test]
    fn level_features_read_the_options() {
        let mut env = env();
        let mut manager = BoostManager::new();
        let level_1 = manager.get_chat_boost_level_features_object(&mut env, false, 1);
        assert_eq!(level_1.level, 1);
        assert!(level_1.can_set_custom_background);
        assert!(!level_1.can_recognize_speech);
        let level_2 = manager.get_chat_boost_level_features_object(&mut env, false, 2);
        assert!(level_2.can_recognize_speech);
        let level_5 = manager.get_chat_boost_level_features_object(&mut env, false, 5);
        assert_eq!(level_5.story_per_day_count, 2, "the clamp to the max level");
    }

    #[test]
    fn features_build_the_level_list_and_mins() {
        let mut env = env();
        let mut manager = BoostManager::new();
        let features = manager.get_chat_boost_features_object(&mut env, false);
        assert_eq!(features.features.len(), 2);
        assert_eq!(features.min_custom_background_boost_level, 1);
        assert_eq!(features.min_speech_recognition_boost_level, 2);
    }

    #[test]
    fn links_build_public_and_private_forms() {
        let mut env = env();
        let mut manager = BoostManager::new();
        let channel_id = ChannelId::new(1000); // channel ids are the positive band
        let (link, is_public) = manager
            .get_dialog_boost_link(&mut env, DialogId::from_channel_id(channel_id))
            .unwrap();
        assert!(!is_public);
        assert_eq!(link, format!("https://t.me/boost?c={}", channel_id.get()));

        env.username = "durov".to_string();
        let (link, is_public) = manager
            .get_dialog_boost_link(&mut env, DialogId::from_channel_id(channel_id))
            .unwrap();
        assert!(is_public);
        assert_eq!(link, "https://t.me/boost/durov");
    }

    #[test]
    fn the_apply_flow_parks_and_redirects_the_empty_slots() {
        // `boost_dialog` (cpp:427-433): the gate error settles the
        // promise, the empty slot list redirects to the slots family and
        // the slot list parks at the apply lot (the net-inert posture)
        let mut env = env();
        let mut manager = BoostManager::new();
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let capture = std::sync::Arc::clone(&slot);
        let promise = tdutils::promise::PromiseCreator::lambda_result(
            move |result: tdutils::status::Result<td_api::ChatBoostSlots>| {
                *capture.lock().unwrap() = Some(match result {
                    tdutils::status::Result::Err(status) => Some(status.message().to_string()),
                    tdutils::status::Result::Ok(_) => None,
                });
            },
        );
        manager.boost_dialog(&mut env, DialogId::new(0), &[1], promise);
        assert_eq!(
            *slot.lock().unwrap(),
            Some(Some("Chat not found".to_string()))
        );
        assert!(manager.pending_apply_boost.is_empty());

        // the empty slot list redirects to the slots family
        manager.boost_dialog(&mut env, DialogId::new(1), &[], Promise::default());
        assert_eq!(manager.pending_get_my_boosts.len(), 1);
        assert!(manager.pending_apply_boost.is_empty());

        manager.boost_dialog(&mut env, DialogId::new(1), &[1, 2], Promise::default());
        manager.boost_dialog(&mut env, DialogId::new(1), &[3], Promise::default());
        assert_eq!(manager.pending_apply_boost.len(), 2);
        assert!(manager.pop_apply_boost_promise().is_some());
        assert!(manager.pop_apply_boost_promise().is_some());
        assert!(manager.pop_apply_boost_promise().is_none(), "the FIFO lot");
    }

    #[test]
    fn get_boost_slots_parks_and_pops_fifo() {
        // the net-inert posture: no handle, so the promise parks and no
        // wire query goes out (the pre-init windows)
        let mut env = env();
        let mut manager = BoostManager::new();
        assert!(manager.net.is_none());
        manager.get_boost_slots(&mut env, Promise::default());
        manager.get_boost_slots(&mut env, Promise::default());
        assert_eq!(manager.pending_get_my_boosts.len(), 2);
        assert!(manager.pop_get_my_boosts_promise().is_some());
        assert!(manager.pop_get_my_boosts_promise().is_some());
        assert!(
            manager.pop_get_my_boosts_promise().is_none(),
            "the FIFO lot"
        );
    }

    #[test]
    fn the_status_conversion_runs_the_ladder_and_the_audience_band() {
        // `get_chat_boost_status_object` (cpp:135-191): the validation
        // ladder rewrites the invalid fields, the audience counts feed the
        // participant-count band on a channel and the percentage, and the
        // gift count clamps to the boost count
        let mut env = env();
        let mut manager = BoostManager::new();
        let channel_id = ChannelId::new(1000);
        let status = manager.get_chat_boost_status_object(
            &mut env,
            DialogId::from_channel_id(channel_id),
            telegram_api::premium_BoostsStatus {
                flags: 31,
                my_boost: true,
                level: -1,
                current_level_boosts: 5,
                boosts: 3,
                gift_boosts: 9,
                next_level_boosts: 4,
                premium_audience: Some(telegram_api::StatsPercentValue {
                    part: -1.5,
                    total: 0.0,
                }),
                boost_url: "https://t.me/boost/x".to_string(),
                prepaid_giveaways: Vec::new(),
                my_boost_slots: vec![7],
            },
        );
        // the ladder: level 0, boosts lifted to the level floor, the
        // next level bumped past it
        assert_eq!(
            (
                status.level,
                status.current_level_boost_count,
                status.boost_count,
                status.next_level_boost_count
            ),
            (0, 5, 5, 6)
        );
        // the audience: the negative part clamps to zero and the
        // participant count is the part/total max, so the percentage
        // stays zero — the channel band still runs (cpp:161-163)
        assert_eq!(
            (
                status.premium_member_count,
                status.premium_member_percentage
            ),
            (0, 0.0)
        );
        assert_eq!(env.participant_counts, vec![(channel_id.get(), 0)]);
        assert_eq!(status.gift_code_boost_count, 5, "the gift-count clamp");
        assert_eq!(status.applied_slot_ids, vec![7]);

        // the user dialog skips the channel band
        env.participant_counts.clear();
        let status = manager.get_chat_boost_status_object(
            &mut env,
            DialogId::new(700000001),
            telegram_api::premium_BoostsStatus {
                flags: 31,
                my_boost: true,
                level: 2,
                current_level_boosts: 1,
                boosts: 4,
                gift_boosts: 9,
                next_level_boosts: 10,
                premium_audience: Some(telegram_api::StatsPercentValue {
                    part: 2.5,
                    total: 10.0,
                }),
                boost_url: String::new(),
                prepaid_giveaways: Vec::new(),
                my_boost_slots: Vec::new(),
            },
        );
        assert!(env.participant_counts.is_empty(), "the user dialog band");
        assert_eq!(
            (
                status.premium_member_count,
                status.premium_member_percentage
            ),
            (2, 20.0)
        );
        assert_eq!(status.gift_code_boost_count, 4, "the gift-count clamp");
    }

    #[test]
    fn the_status_gate_answers_the_access_band_and_the_send_parks() {
        // `get_dialog_boost_status` (cpp:420-426): the gate error settles
        // the promise (the TRY_STATUS_PROMISE shape), the passing flow
        // parks the promise at the family lot (the net-inert posture —
        // no handle, so no wire query), and the FIFO pop drains it
        let mut env = env();
        let mut manager = BoostManager::new();
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let capture = std::sync::Arc::clone(&slot);
        let promise = tdutils::promise::PromiseCreator::lambda_result(
            move |result: tdutils::status::Result<td_api::ChatBoostStatus>| {
                *capture.lock().unwrap() = Some(match result {
                    tdutils::status::Result::Err(status) => Some(status.message().to_string()),
                    tdutils::status::Result::Ok(_) => None,
                });
            },
        );
        manager.get_dialog_boost_status(&mut env, DialogId::new(0), promise);
        assert_eq!(
            *slot.lock().unwrap(),
            Some(Some("Chat not found".to_string()))
        );
        assert!(manager.pending_get_boosts_status.is_empty());

        manager.get_dialog_boost_status(&mut env, DialogId::new(1), Promise::default());
        manager.get_dialog_boost_status(&mut env, DialogId::new(1), Promise::default());
        assert_eq!(manager.pending_get_boosts_status.len(), 2);
        assert!(manager.pop_get_boosts_status_promise().is_some());
        assert!(manager.pop_get_boosts_status_promise().is_some());
        assert!(
            manager.pop_get_boosts_status_promise().is_none(),
            "the FIFO lot"
        );
    }

    #[test]
    fn the_slots_conversion_runs_the_cxx_bands() {
        // `get_chat_boost_slots_object` (cpp:47-93): the users/chats
        // intakes run first, the expiry filter drops, the invalid peer
        // drops, the no-peer slot zeroes the dates and the live peer
        // force-creates its dialog
        let mut env = env();
        let mut manager = BoostManager::new();
        let user_dialog_id = DialogId::new(700000001);
        let slots = manager.get_chat_boost_slots_object(
            &mut env,
            telegram_api::premium_MyBoosts {
                my_boosts: vec![
                    telegram_api::MyBoost {
                        flags: 3,
                        slot: 3,
                        peer: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                            user_id: 700000001,
                        })),
                        date: -5,
                        expires: 2000,
                        cooldown_until_date: -7,
                    },
                    telegram_api::MyBoost {
                        flags: 3,
                        slot: 4,
                        peer: None,
                        date: 100,
                        expires: 2000,
                        cooldown_until_date: 50,
                    },
                    telegram_api::MyBoost {
                        flags: 3,
                        slot: 5,
                        peer: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                            user_id: 700000002,
                        })),
                        date: 100,
                        expires: 1000,
                        cooldown_until_date: 50,
                    },
                    telegram_api::MyBoost {
                        flags: 1,
                        slot: 6,
                        peer: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                            user_id: 0,
                        })),
                        date: 100,
                        expires: 2000,
                        cooldown_until_date: 0,
                    },
                ],
                chats: vec![telegram_api::Chat::chatEmpty(telegram_api::chatEmpty {
                    id: 1,
                })],
                users: vec![telegram_api::User::userEmpty(telegram_api::userEmpty {
                    id: 700000001,
                })],
            },
        );
        assert_eq!(slots.slots.len(), 2, "the expired + invalid slots drop");
        // the negative dates clamp to zero (cpp:61-62)
        assert_eq!(
            (
                slots.slots[0].slot_id,
                slots.slots[0].currently_boosted_chat_id,
                slots.slots[0].start_date,
                slots.slots[0].expiration_date,
                slots.slots[0].cooldown_until_date
            ),
            (3, 700000001, 0, 2000, 0)
        );
        // the empty slot zeroes the dates (cpp:78-81)
        assert_eq!(
            (
                slots.slots[1].slot_id,
                slots.slots[1].currently_boosted_chat_id,
                slots.slots[1].start_date,
                slots.slots[1].cooldown_until_date
            ),
            (4, 0, 0, 0)
        );
        assert_eq!(env.intakes[0], "users 1 GetMyBoostsQuery");
        assert_eq!(env.intakes[1], "chats 1 GetMyBoostsQuery");
        assert_eq!(env.intakes[2], "force GetMyBoostsQuery");
        assert_eq!(env.forced_dialogs, vec![user_dialog_id]);
    }
}
