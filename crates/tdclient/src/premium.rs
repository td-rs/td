//! `Premium` (td/telegram/Premium.{h,cpp}) — the M.36 Wave-3 premium
//! manager plane. The value surface (check_payment_amount, the premium
//! limits table, the gift-option helpers) is the real port; the queries
//! ride the client-side dispatcher under their `PremiumNetKind` — the
//! landed flows are `payments.applyGiftCode` (the Updates answer
//! re-enters `on_get_updates` with the promise) and
//! `payments.getPremiumGiftCodeOptions` (the filter/star-price/discount
//! conversion at [`PremiumManager::on_get_premium_gift_options`], the
//! sticker leg parked at the stickers plane). The env is the whole-Td
//! shape (the [`crate::td::PremiumTdEnv`] widening — the PrivacyEnv
//! convention), so the remaining flows (giveaway options/info,
//! checkGiftCode, the store-purpose trio, the promo) resolve
//! peers/chats/users through the same trait.

use crate::chat_manager::{ChannelId, DialogParticipantStatus};
use crate::dialog_id::DialogId;
use crate::td_api;
use crate::user_manager::{AccessRights, UserId};
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::{Result, Status};

/// `check_payment_amount` (Premium.cpp:184-192).
pub fn check_payment_amount(currency: &str, amount: i64) -> Result<()> {
    if amount <= 0 || !crate::misc::check_currency_amount(amount) {
        return Result::Err(Status::error(
            400,
            "Invalid amount of the currency specified",
        ));
    }
    if crate::misc::clean_input_string(currency).is_none() {
        return Result::Err(Status::error(400, "Strings must be encoded in UTF-8"));
    }
    Result::Ok(())
}

/// `get_premium_limit_keys` (Premium.cpp:932-957).
pub fn get_premium_limit_keys() -> &'static [&'static str] {
    &[
        "channels",
        "saved_gifs",
        "stickers_faved",
        "dialog_filters",
        "dialog_filters_chats",
        "dialogs_pinned",
        "dialogs_folder_pinned",
        "channels_public",
        "caption_length",
        "about_length",
        "chatlist_invites",
        "chatlists_joined",
        "story_expiring",
        "story_caption_length",
        "stories_sent_weekly",
        "stories_sent_monthly",
        "stories_suggested_reactions",
        "recommended_channels",
        "saved_dialogs_pinned",
        "bots_create",
        "aicompose_tone_saved",
        "message_length",
    ]
}

/// `get_limit_type_key` (Premium.cpp:958-1015) — the key side of the
/// limit-type mapping.
pub fn get_limit_type_key(limit_type: &td_api::PremiumLimitType) -> &'static str {
    match limit_type {
        td_api::PremiumLimitType::premiumLimitTypeSupergroupCount(_) => "channels",
        td_api::PremiumLimitType::premiumLimitTypeSavedAnimationCount(_) => "saved_gifs",
        td_api::PremiumLimitType::premiumLimitTypeFavoriteStickerCount(_) => "stickers_faved",
        td_api::PremiumLimitType::premiumLimitTypeChatFolderCount(_) => "dialog_filters",
        td_api::PremiumLimitType::premiumLimitTypeChatFolderChosenChatCount(_) => {
            "dialog_filters_chats"
        }
        td_api::PremiumLimitType::premiumLimitTypePinnedChatCount(_) => "dialogs_pinned",
        td_api::PremiumLimitType::premiumLimitTypePinnedArchivedChatCount(_) => {
            "dialogs_folder_pinned"
        }
        td_api::PremiumLimitType::premiumLimitTypePinnedSavedMessagesTopicCount(_) => {
            "saved_dialogs_pinned"
        }
        td_api::PremiumLimitType::premiumLimitTypeCreatedPublicChatCount(_) => "channels_public",
        td_api::PremiumLimitType::premiumLimitTypeCaptionLength(_) => "caption_length",
        td_api::PremiumLimitType::premiumLimitTypeBioLength(_) => "about_length",
        td_api::PremiumLimitType::premiumLimitTypeChatFolderInviteLinkCount(_) => {
            "chatlist_invites"
        }
        td_api::PremiumLimitType::premiumLimitTypeShareableChatFolderCount(_) => "chatlists_joined",
        td_api::PremiumLimitType::premiumLimitTypeActiveStoryCount(_) => "story_expiring",
        td_api::PremiumLimitType::premiumLimitTypeStoryCaptionLength(_) => "story_caption_length",
        td_api::PremiumLimitType::premiumLimitTypeWeeklyPostedStoryCount(_) => {
            "stories_sent_weekly"
        }
        td_api::PremiumLimitType::premiumLimitTypeMonthlyPostedStoryCount(_) => {
            "stories_sent_monthly"
        }
        td_api::PremiumLimitType::premiumLimitTypeStorySuggestedReactionAreaCount(_) => {
            "stories_suggested_reactions"
        }
        td_api::PremiumLimitType::premiumLimitTypeSimilarChatCount(_) => "recommended_channels",
        td_api::PremiumLimitType::premiumLimitTypeOwnedBotCount(_) => "bots_create",
        td_api::PremiumLimitType::premiumLimitTypeCustomTextCompositionStyleCount(_) => {
            "aicompose_tone_saved"
        }
        td_api::PremiumLimitType::premiumLimitTypeMessageTextLength(_) => "message_length",
    }
}

/// `get_premium_limit_object` (Premium.cpp:1185-1263) — `None` when the
/// limits are not configured.
pub fn get_premium_limit_object(
    env: &impl PremiumEnv,
    limit_type: &td_api::PremiumLimitType,
) -> Option<td_api::PremiumLimit> {
    let key = get_limit_type_key(limit_type);
    let default_limit = env.get_option_integer(&format!("{key}_limit_default")) as i32;
    let premium_limit = env.get_option_integer(&format!("{key}_limit_premium")) as i32;
    if default_limit <= 0 || premium_limit <= default_limit {
        return None;
    }
    Some(td_api::PremiumLimit {
        r#type: Some(limit_type.clone()),
        default_value: default_limit,
        premium_value: premium_limit,
    })
}

/// The `G()`/`td_` surface of the premium plane — the options reads plus
/// the whole-Td resolution methods the C++ free functions over `Td`
/// need (the §3 step 1b widening; the arms mirror the landed
/// `PrivacyEnv`/`PaymentsEnv`/`TdSponsoredEnv` shapes over `Td`).
pub trait PremiumEnv {
    fn get_option_integer(&self, name: &str) -> i64;

    fn get_option_string(&self, name: &str, default_value: &str) -> String;

    fn send_update(&mut self, update: td_api::Update);

    // ---- the DialogManager/ChatManager/UserManager reads ----

    /// `td_->dialog_manager_->have_dialog_force(dialog_id, source)`
    /// (DialogManager.cpp:1536).
    fn have_dialog_force(&mut self, dialog_id: DialogId, source: &str) -> bool;

    /// `td_->chat_manager_->get_channel_status(channel_id)`
    /// (ChatManager.cpp).
    fn get_channel_status(&mut self, channel_id: ChannelId) -> DialogParticipantStatus;

    /// `td_->dialog_manager_->get_input_peer(dialog_id, access_rights)`
    /// (DialogManager.cpp:1377).
    fn get_input_peer(
        &mut self,
        dialog_id: DialogId,
        access_rights: AccessRights,
    ) -> Option<telegram_api::InputPeer>;

    /// `td_->dialog_manager_->on_get_dialog_error(dialog_id, status,
    /// source)` (DialogManager.cpp:1996) — the ResultHandler `on_error`
    /// intake.
    fn on_get_dialog_error(&mut self, dialog_id: DialogId, status: &Status, source: &str);

    /// `td_->user_manager_->on_get_users(users, source)`.
    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str);

    /// `td_->chat_manager_->on_get_chats(chats, source)`.
    fn on_get_chats(&mut self, chats: Vec<telegram_api::Chat>, source: &str);

    /// `td_->chat_manager_->have_channel_force(channel_id, source)`
    /// (ChatManager.cpp:4963).
    fn have_channel_force(&mut self, channel_id: ChannelId, source: &str) -> bool;

    /// `td_->dialog_manager_->force_create_dialog(dialog_id, source,
    /// force_read)` (DialogManager.cpp:1540).
    fn force_create_dialog(&mut self, dialog_id: DialogId, source: &str, force_read: bool);

    /// `td_->user_manager_->get_input_user(user_id)` (UserManager.cpp).
    fn get_input_user(&mut self, user_id: UserId) -> Result<telegram_api::InputUser>;

    /// `td_->dialog_manager_->have_dialog_info_force(dialog_id, source)`
    /// (DialogManager.cpp:1596).
    fn have_dialog_info_force(&mut self, dialog_id: DialogId, source: &str) -> bool;

    /// `get_message_sender_object(td, dialog_id, source)` — the free
    /// function over the managers (dialog_manager).
    fn get_message_sender_object(
        &mut self,
        dialog_id: DialogId,
        source: &str,
    ) -> td_api::MessageSender;

    /// `td_->user_manager_->get_user_id_object(user_id, source)`.
    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64;

    // ---- the query sends — the dispatcher arms live at the Td env ----
    fn send_get_premium_promo_query(&mut self) {
        tdutils::log_info!("Send help.getPremiumPromo — [net-inert]");
    }
    fn send_get_premium_gift_options_query(&mut self) {
        tdutils::log_info!("Send payments.getPremiumGiftCodeOptions — [net-inert]");
    }
    fn send_get_premium_giveaway_options_query(
        &mut self,
        _boost_peer: telegram_api::InputPeer,
        boosted_dialog_id: i64,
    ) {
        tdutils::log_info!(
            "Send payments.getPremiumGiftCodeOptions for {boosted_dialog_id} — [net-inert]"
        );
    }
    fn send_check_gift_code_query(&mut self, code: &str) {
        tdutils::log_info!("Send payments.checkGiftCode for {code} — [net-inert]");
    }
    fn send_apply_gift_code_query(&mut self, code: &str) {
        tdutils::log_info!("Send payments.applyGiftCode for {code} — [net-inert]");
    }
    fn send_premium_gift_query(&mut self, user_id: i64, star_count: i64, month_count: i32) {
        tdutils::log_info!(
            "Send payments.sendStarsGift(user_id = {user_id}, star_count = {star_count}, month_count = {month_count}) — [blocked: net]"
        );
    }
    fn send_launch_prepaid_giveaway_query(
        &mut self,
        giveaway_id: i64,
        user_count: i32,
        star_count: i64,
    ) {
        tdutils::log_info!(
            "Send payments.launchPrepaidGiveaway({giveaway_id}, users = {user_count}, stars = {star_count}) — [blocked: net]"
        );
    }
    fn send_get_giveaway_info_query(
        &mut self,
        dialog_id: i64,
        _peer: telegram_api::InputPeer,
        _msg_id: i32,
    ) {
        tdutils::log_info!("Send payments.getGiveawayInfo({dialog_id}) — [net-inert]");
    }
    fn send_can_purchase_premium_query(
        &mut self,
        _purpose: telegram_api::InputStorePaymentPurpose,
    ) {
        tdutils::log_info!("Send payments.canPurchaseStore — [net-inert]");
    }
    fn send_assign_store_transaction_query(
        &mut self,
        _receipt: Vec<u8>,
        _purpose: telegram_api::InputStorePaymentPurpose,
        _is_app_store: bool,
    ) {
        tdutils::log_info!("Send the store-transaction assign — [net-inert]");
    }

    // --- the store-purpose conversion + the suggested-action seam ---

    /// `get_input_store_payment_purpose` (Premium.cpp:220-300) over the
    /// payments surface — the Td env builds the field-split payments env
    /// and runs the crate conversion; the default keeps the blocked
    /// posture for the test envs.
    fn get_input_store_payment_purpose(
        &mut self,
        purpose: Option<td_api::StorePaymentPurpose>,
    ) -> Result<telegram_api::InputStorePaymentPurpose> {
        let _ = purpose;
        tdutils::log_info!("get_input_store_payment_purpose — [blocked: payments plane]");
        Result::from_status(Status::error(500, "Not implemented yet"))
    }

    /// `dismiss_suggested_action(SuggestedAction{type}, Promise<Unit>())`
    /// — the premium-subscription assign legs (Premium.cpp:1461-1465);
    /// the suggested-action plane parks at its own seam.
    fn dismiss_suggested_action(
        &mut self,
        action_type: crate::suggested_action::SuggestedActionType,
    ) {
        tdutils::log_info!(
            "dismiss_suggested_action({action_type:?}) — [blocked: suggested-action plane]"
        );
    }
}

/// `get_boost_input_peer` (Premium.cpp:194-207) — the premium-plane copy
/// over [`PremiumEnv`] (the payments plane keeps its own
/// `crate::payments::get_boost_input_peer` over `PaymentsEnv` — the
/// env-trait arm-duplication convention; this is the C++ anchor home).
pub(crate) fn get_boost_input_peer(
    env: &mut impl PremiumEnv,
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
    match env.get_input_peer(dialog_id, AccessRights::Write) {
        Some(peer) => Result::Ok(peer),
        // the C++ CHECK(input_peer != nullptr)
        None => panic!("get_boost_input_peer has no input peer"),
    }
}

/// `get_input_store_payment_purpose` (Premium.cpp:220-300) — the
/// td_api::StorePaymentPurpose → wire conversion over the payments
/// surface (its dependency legs — `get_input_user`, `check_dialog_access`,
/// `get_boost_input_peer`, the `GiveawayParameters` builders,
/// `get_premium_gift_text`, `dismiss_suggested_action` — all live on
/// `PaymentsEnv` already).
pub(crate) fn get_input_store_payment_purpose(
    env: &mut impl crate::payments::PaymentsEnv,
    purpose: Option<td_api::StorePaymentPurpose>,
) -> Result<telegram_api::InputStorePaymentPurpose> {
    use crate::payments::GiveawayParameters;
    use crate::td_api::StorePaymentPurpose;
    let Some(purpose) = purpose else {
        return Result::from_status(Status::error(400, "Purchase purpose must be non-empty"));
    };
    match purpose {
        StorePaymentPurpose::storePaymentPurposePremiumSubscription(p) => Result::Ok(
            telegram_api::InputStorePaymentPurpose::inputStorePaymentPremiumSubscription(
                telegram_api::inputStorePaymentPremiumSubscription {
                    flags: 0,
                    restore: p.is_restore,
                    upgrade: p.is_upgrade,
                },
            ),
        ),
        StorePaymentPurpose::storePaymentPurposePremiumGift(p) => {
            let input_user = match env.get_input_user(UserId::new(p.user_id)) {
                Result::Ok(input_user) => input_user,
                Result::Err(status) => return Result::Err(status),
            };
            if let Result::Err(status) = check_payment_amount(&p.currency, p.amount) {
                return Result::Err(status);
            }
            let text = match env.premium_gift_text(p.text) {
                Result::Ok(text) => text,
                Result::Err(status) => return Result::Err(status),
            };
            let mut flags = 0;
            if text.is_some() {
                flags |= crate::payments::INPUT_STORE_PREMIUM_GIFT_CODE_MESSAGE_MASK;
            }
            Result::Ok(
                telegram_api::InputStorePaymentPurpose::inputStorePaymentPremiumGiftCode(
                    telegram_api::inputStorePaymentPremiumGiftCode {
                        flags,
                        users: vec![input_user],
                        boost_peer: None,
                        currency: p.currency,
                        amount: p.amount,
                        message: text,
                    },
                ),
            )
        }
        StorePaymentPurpose::storePaymentPurposePremiumGiftCodes(p) => {
            let mut input_users = Vec::with_capacity(p.user_ids.len());
            for &user_id in &p.user_ids {
                match env.get_input_user(UserId::new(user_id)) {
                    Result::Ok(input_user) => input_users.push(input_user),
                    Result::Err(status) => return Result::Err(status),
                }
            }
            if let Result::Err(status) = check_payment_amount(&p.currency, p.amount) {
                return Result::Err(status);
            }
            let boost_input_peer = match crate::payments::get_boost_input_peer(
                env,
                DialogId::new(p.boosted_chat_id),
            ) {
                Result::Ok(peer) => peer,
                Result::Err(status) => return Result::Err(status),
            };
            let text = match env.premium_gift_text(p.text) {
                Result::Ok(text) => text,
                Result::Err(status) => return Result::Err(status),
            };
            let mut flags = crate::payments::INPUT_STORE_PREMIUM_GIFT_CODE_BOOST_PEER_MASK;
            if text.is_some() {
                flags |= crate::payments::INPUT_STORE_PREMIUM_GIFT_CODE_MESSAGE_MASK;
            }
            Result::Ok(
                telegram_api::InputStorePaymentPurpose::inputStorePaymentPremiumGiftCode(
                    telegram_api::inputStorePaymentPremiumGiftCode {
                        flags,
                        users: input_users,
                        boost_peer: Some(Box::new(boost_input_peer)),
                        currency: p.currency,
                        amount: p.amount,
                        message: text,
                    },
                ),
            )
        }
        StorePaymentPurpose::storePaymentPurposePremiumGiveaway(p) => {
            if let Result::Err(status) = check_payment_amount(&p.currency, p.amount) {
                return Result::Err(status);
            }
            let parameters =
                match GiveawayParameters::get_giveaway_parameters(env, p.parameters.as_ref()) {
                    Result::Ok(parameters) => parameters,
                    Result::Err(status) => return Result::Err(status),
                };
            Result::Ok(parameters.get_input_store_payment_premium_giveaway(
                env,
                &p.currency,
                p.amount,
            ))
        }
        StorePaymentPurpose::storePaymentPurposeStarGiveaway(p) => {
            if let Result::Err(status) = check_payment_amount(&p.currency, p.amount) {
                return Result::Err(status);
            }
            if p.winner_count <= 0 || p.star_count <= 0 {
                return Result::from_status(Status::error(
                    400,
                    "Invalid giveaway parameters specified",
                ));
            }
            let parameters =
                match GiveawayParameters::get_giveaway_parameters(env, p.parameters.as_ref()) {
                    Result::Ok(parameters) => parameters,
                    Result::Err(status) => return Result::Err(status),
                };
            Result::Ok(parameters.get_input_store_payment_stars_giveaway(
                env,
                &p.currency,
                p.amount,
                p.winner_count,
                p.star_count,
            ))
        }
        StorePaymentPurpose::storePaymentPurposeStars(p) => {
            let mut flags = 0;
            let mut spend_purpose_input_peer = None;
            let spend_dialog_id = DialogId::new(p.chat_id);
            if spend_dialog_id != DialogId::default() {
                if let Result::Err(status) =
                    env.check_dialog_access(spend_dialog_id, "storePaymentPurposeStars")
                {
                    return Result::Err(status);
                }
                match env.get_input_peer(spend_dialog_id, AccessRights::Read) {
                    Some(peer) => {
                        spend_purpose_input_peer = Some(peer);
                        flags |= crate::payments::INPUT_STORE_STARS_TOPUP_SPEND_PURPOSE_PEER_MASK;
                    }
                    // the C++ CHECK(spend_purpose_input_peer != nullptr)
                    None => panic!("storePaymentPurposeStars has no input peer"),
                }
            }
            if let Result::Err(status) = check_payment_amount(&p.currency, p.amount) {
                return Result::Err(status);
            }
            env.dismiss_suggested_action();
            Result::Ok(
                telegram_api::InputStorePaymentPurpose::inputStorePaymentStarsTopup(
                    telegram_api::inputStorePaymentStarsTopup {
                        flags,
                        stars: p.star_count,
                        currency: p.currency,
                        amount: p.amount,
                        spend_purpose_peer: spend_purpose_input_peer.map(Box::new),
                    },
                ),
            )
        }
        StorePaymentPurpose::storePaymentPurposeGiftedStars(p) => {
            let input_user = match env.get_input_user(UserId::new(p.user_id)) {
                Result::Ok(input_user) => input_user,
                Result::Err(status) => return Result::Err(status),
            };
            if let Result::Err(status) = check_payment_amount(&p.currency, p.amount) {
                return Result::Err(status);
            }
            Result::Ok(
                telegram_api::InputStorePaymentPurpose::inputStorePaymentStarsGift(
                    telegram_api::inputStorePaymentStarsGift {
                        user_id: Some(input_user),
                        stars: p.star_count,
                        currency: p.currency,
                        amount: p.amount,
                    },
                ),
            )
        }
    }
}

/// The premium query kinds riding the dispatcher — the C++
/// ResultHandler classes of Premium.cpp (the landed flows; the rest of
/// the twelve join in follow-up slices).
#[derive(Clone, Copy, Debug)]
pub(crate) enum PremiumNetKind {
    /// `ApplyGiftCodeQuery` (cpp:580) — the Updates answer re-enters
    /// `on_get_updates` with the promise.
    ApplyGiftCode,
    /// `GetPremiumGiftOptionsQuery` (cpp:395) — the gift option vector
    /// converts at [`PremiumManager::on_get_premium_gift_options`].
    GetPremiumGiftCodeOptions,
    /// `GetPremiumGiveawayOptionsQuery` (cpp:458) — the boosted twin; the
    /// kind carries the boosted dialog for the `on_error` tail.
    GetPremiumGiveawayOptions { boosted_dialog_id: i64 },
    /// `GetGiveawayInfoQuery` (cpp:726) — the kind carries the dialog for
    /// the `on_error` tail.
    GetGiveawayInfo { dialog_id: i64 },
    /// `CheckGiftCodeQuery` (cpp:512) — the slug rides the query; the
    /// users/chats intakes run at the answer.
    CheckGiftCode,
    /// `CanPurchasePremiumQuery` (cpp:826) — the Bool answer band.
    CanPurchasePremium,
    /// `AssignAppStoreTransactionQuery`/`AssignPlayMarketTransactionQuery`
    /// (cpp:860/893) — the Updates answers re-enter `on_get_updates`.
    AssignStoreTransaction { is_app_store: bool },
}

/// The collapsed `PremiumManager` — a router over the query seams.
#[derive(Default)]
pub struct PremiumManager {
    /// The `PremiumState`-typed promises parked at the state seam.
    pub parked_premium_state: Vec<Promise<td_api::PremiumState>>,
    /// The `PremiumFeatures`-typed promises parked at the features seam.
    pub parked_premium_features: Vec<Promise<td_api::PremiumFeatures>>,
    /// The `BusinessFeatures`-typed promises parked at the business
    /// features seam.
    pub parked_business_features: Vec<Promise<td_api::BusinessFeatures>>,
    /// The `PremiumGiftPaymentOptions`-typed promises parked at the gift
    /// options seam.
    pub parked_gift_payment_options: Vec<Promise<td_api::PremiumGiftPaymentOptions>>,
    /// The `PremiumGiveawayPaymentOptions`-typed promises parked at the
    /// giveaway options seam.
    pub parked_giveaway_options: Vec<Promise<td_api::PremiumGiveawayPaymentOptions>>,
    /// The `PremiumGiftCodeInfo`-typed promises parked at the code-check
    /// seam.
    pub parked_gift_code_infos: Vec<Promise<td_api::PremiumGiftCodeInfo>>,
    /// The `GiveawayInfo`-typed promises parked at the giveaway-info
    /// seam.
    pub parked_giveaway_infos: Vec<Promise<td_api::GiveawayInfo>>,
    /// The `()`-typed promises parked at the action seams.
    pub net_blocked_promises: Vec<Promise<()>>,
    /// The `()`-typed promises parked at the apply-gift-code flow (the
    /// C++ `ApplyGiftCodeQuery` promise chains into `on_get_updates`).
    pub(crate) pending_apply_gift_code: Vec<Promise<()>>,
    /// The `()`-typed promises parked at the can-purchase flow (the C++
    /// `CanPurchasePremiumQuery` promise).
    pub(crate) pending_can_purchase: Vec<Promise<()>>,
    /// The `()`-typed promises parked at the store-transaction assigns
    /// (the C++ handler promises chain into `on_get_updates`).
    pub(crate) pending_assign_store_transaction: Vec<Promise<()>>,
}

impl PremiumManager {
    pub fn new() -> PremiumManager {
        PremiumManager::default()
    }

    /// `get_premium_limit` (Premium.cpp:1264-1271) — the local answer.
    pub fn get_premium_limit(
        &self,
        env: &impl PremiumEnv,
        limit_type: &Option<td_api::PremiumLimitType>,
        mut promise: Promise<td_api::PremiumLimit>,
    ) {
        let Some(limit_type) = limit_type else {
            return promise.set_error(Status::error(400, "Limit type must be non-empty"));
        };
        match get_premium_limit_object(env, limit_type) {
            Some(limit) => promise.set_value(limit),
            None => promise.set_error(Status::error(404, "Not Found")),
        }
    }

    /// `get_premium_state` (Premium.cpp) — the promo query is the seam.
    pub fn get_premium_state(
        &mut self,
        env: &mut impl PremiumEnv,
        promise: Promise<td_api::PremiumState>,
    ) {
        env.send_get_premium_promo_query();
        self.parked_premium_state.push(promise);
    }

    /// `get_premium_features` (Premium.cpp:1273-1291) — the local list
    /// from the `premium_features` option; the per-feature objects join
    /// the feature builders (recorded).
    pub fn get_premium_features(
        &self,
        env: &impl PremiumEnv,
        promise: Promise<td_api::PremiumFeatures>,
    ) {
        let features = env
            .get_option_string(
                "premium_features",
                "stories,more_upload,double_limits,business,last_seen,voice_to_text,faster_download,\
                 translations,animated_emoji,emoji_status,saved_tags,peer_colors,wallpapers,profile_badge,\
                 message_privacy,advanced_chat_management,no_ads,app_icons,infinite_reactions,animated_userpics,\
                 premium_stickers,effects,todo,paid_messages,pm_noforwards,ai_compose,rich_formatting",
            )
            .split(',')
            .filter_map(get_premium_feature_object)
            .collect();
        let mut promise = promise;
        promise.set_value(td_api::PremiumFeatures {
            features,
            limits: Vec::new(),
            payment_link: None,
        });
    }

    /// `get_business_features` (Premium.cpp) — the query is the seam.
    pub fn get_business_features(
        &mut self,
        env: &mut impl PremiumEnv,
        promise: Promise<td_api::BusinessFeatures>,
    ) {
        let _ = env;
        self.parked_business_features.push(promise);
    }

    /// `get_premium_gift_payment_options` (Premium.cpp) — the query is
    /// the seam.
    pub fn get_premium_gift_payment_options(
        &mut self,
        env: &mut impl PremiumEnv,
        promise: Promise<td_api::PremiumGiftPaymentOptions>,
    ) {
        env.send_get_premium_gift_options_query();
        self.parked_gift_payment_options.push(promise);
    }

    /// `get_premium_giveaway_options` (Premium.cpp:1395-1397) →
    /// `GetPremiumGiveawayOptionsQuery::send` (cpp:471-481) — the boost
    /// peer resolves at the send leg over the widened env; the error arm
    /// is the handler's `on_error` (cpp:505-509): the dialog-error tail,
    /// then `set_error`.
    pub fn get_premium_giveaway_options(
        &mut self,
        env: &mut impl PremiumEnv,
        boosted_dialog_id: i64,
        mut promise: Promise<td_api::PremiumGiveawayPaymentOptions>,
    ) {
        let dialog_id = DialogId::new(boosted_dialog_id);
        let boost_input_peer = match get_boost_input_peer(env, dialog_id) {
            Result::Ok(peer) => peer,
            Result::Err(status) => {
                env.on_get_dialog_error(dialog_id, &status, "GetPremiumGiveawayOptionsQuery");
                return promise.set_error(status);
            }
        };
        env.send_get_premium_giveaway_options_query(boost_input_peer, boosted_dialog_id);
        self.parked_giveaway_options.push(promise);
    }

    /// `GetPremiumGiveawayOptionsQuery::on_result`'s promise pop
    /// (Premium.cpp:483-503).
    pub(crate) fn take_giveaway_options_promise(
        &mut self,
    ) -> Option<Promise<td_api::PremiumGiveawayPaymentOptions>> {
        if self.parked_giveaway_options.is_empty() {
            None
        } else {
            Some(self.parked_giveaway_options.remove(0))
        }
    }

    /// `GetPremiumGiveawayOptionsQuery::on_result` (Premium.cpp:483-503)
    /// — the giveaway filter (`users > 0 && months > 0 && amount > 0 &&
    /// non-empty currency != "XTR"`), then the
    /// `PremiumGiveawayPaymentOption` build with the `store_quantity`
    /// bands (0 with no store product, else clamped to `>= 1`); no sort.
    /// The error arm is `on_error` (cpp:505-509): the dialog-error tail
    /// for the boosted chat, then `set_error`.
    pub(crate) fn on_get_premium_giveaway_options(
        &mut self,
        env: &mut impl PremiumEnv,
        boosted_dialog_id: i64,
        result: Result<Vec<telegram_api::PremiumGiftCodeOption>>,
        mut promise: Promise<td_api::PremiumGiveawayPaymentOptions>,
    ) {
        let results = match result {
            Result::Err(status) => {
                env.on_get_dialog_error(
                    DialogId::new(boosted_dialog_id),
                    &status,
                    "GetPremiumGiveawayOptionsQuery",
                );
                return promise.set_error(status);
            }
            Result::Ok(results) => results,
        };
        let options = results
            .into_iter()
            .filter(|option| {
                option.users > 0
                    && option.months > 0
                    && option.amount > 0
                    && !option.currency.is_empty()
                    && option.currency != "XTR"
            })
            .map(|mut option| td_api::PremiumGiveawayPaymentOption {
                store_product_quantity: if option.store_product.is_empty() {
                    0
                } else if option.store_quantity <= 0 {
                    1
                } else {
                    option.store_quantity
                },
                currency: std::mem::take(&mut option.currency),
                amount: option.amount,
                winner_count: option.users,
                month_count: option.months,
                store_product_id: option.store_product,
            })
            .collect();
        promise.set_value(td_api::PremiumGiveawayPaymentOptions { options });
    }

    /// `check_premium_gift_code` (Premium.cpp) — the query is the seam.
    pub fn check_premium_gift_code(
        &mut self,
        env: &mut impl PremiumEnv,
        code: &str,
        promise: Promise<td_api::PremiumGiftCodeInfo>,
    ) {
        env.send_check_gift_code_query(code);
        self.parked_gift_code_infos.push(promise);
    }

    /// `apply_premium_gift_code` (Premium.cpp) — the query is the seam.
    pub fn apply_premium_gift_code(
        &mut self,
        env: &mut impl PremiumEnv,
        code: &str,
        promise: Promise<()>,
    ) {
        env.send_apply_gift_code_query(code);
        self.pending_apply_gift_code.push(promise);
    }

    /// `ApplyGiftCodeQuery::on_result`'s promise pop (Premium.cpp:595).
    pub(crate) fn take_apply_gift_code_promise(&mut self) -> Option<Promise<()>> {
        if self.pending_apply_gift_code.is_empty() {
            None
        } else {
            Some(self.pending_apply_gift_code.remove(0))
        }
    }

    /// `CheckGiftCodeQuery::on_result`'s promise pop
    /// (Premium.cpp:521-576).
    pub(crate) fn take_gift_code_info_promise(
        &mut self,
    ) -> Option<Promise<td_api::PremiumGiftCodeInfo>> {
        if self.parked_gift_code_infos.is_empty() {
            None
        } else {
            Some(self.parked_gift_code_infos.remove(0))
        }
    }

    /// `CheckGiftCodeQuery::on_result` (Premium.cpp:521-576) — the
    /// users/chats intakes first, then the value bands (date/days/
    /// used_date), the `from_id` creator resolution
    /// (`have_dialog_info_force` + the non-User `force_create_dialog`),
    /// the `to_id`/`giveaway_msg_id` invalid-value drops (a giveaway
    /// message id survives only behind a Channel creator), the
    /// month-count round-trip (`get_premium_duration_month_count` vs
    /// `get_premium_duration_day_count`), and the object build. The
    /// error arm is the plain `on_error` (cpp:578) — no dialog tail.
    pub(crate) fn on_check_gift_code(
        &mut self,
        env: &mut impl PremiumEnv,
        result: Result<telegram_api::payments_CheckedGiftCode>,
        mut promise: Promise<td_api::PremiumGiftCodeInfo>,
    ) {
        let result = match result {
            Result::Err(status) => return promise.set_error(status),
            Result::Ok(result) => result,
        };
        env.on_get_users(result.users, "CheckGiftCodeQuery");
        env.on_get_chats(result.chats, "CheckGiftCodeQuery");

        if result.date <= 0 || result.days <= 0 || result.used_date < 0 {
            tdutils::log_error!("Receive invalid checked gift code response");
            return promise.set_error(Status::error(500, "Receive invalid response"));
        }

        let mut creator_dialog_id = DialogId::default();
        if let Some(from_id) = &result.from_id {
            creator_dialog_id = DialogId::from_peer(from_id);
            if !creator_dialog_id.is_valid()
                || !env.have_dialog_info_force(creator_dialog_id, "CheckGiftCodeQuery")
            {
                tdutils::log_error!("Receive invalid gift code creator");
                return promise.set_error(Status::error(500, "Receive invalid response"));
            }
            if creator_dialog_id.get_type() != crate::dialog_id::DialogType::User {
                env.force_create_dialog(creator_dialog_id, "CheckGiftCodeQuery", true);
            }
        }
        let mut user_id = UserId::new(result.to_id);
        if !user_id.is_valid() && user_id != UserId::default() {
            tdutils::log_error!("Receive invalid gift code recipient {}", user_id.get());
            user_id = UserId::default();
        }
        let mut message_id = crate::messages_manager::MessageId::from_server_message_id(
            crate::messages_manager::ServerMessageId::new(result.giveaway_msg_id),
        );
        if !message_id.is_valid() && message_id != crate::messages_manager::MessageId::default() {
            tdutils::log_error!(
                "Receive invalid giveaway message id {}",
                result.giveaway_msg_id
            );
            message_id = crate::messages_manager::MessageId::default();
        }
        if message_id != crate::messages_manager::MessageId::default()
            && creator_dialog_id.get_type() != crate::dialog_id::DialogType::Channel
        {
            tdutils::log_error!(
                "Receive giveaway message id {} behind a non-channel creator",
                result.giveaway_msg_id
            );
            message_id = crate::messages_manager::MessageId::default();
        }
        let month_count = crate::message_content::get_premium_duration_month_count(result.days);
        promise.set_value(td_api::PremiumGiftCodeInfo {
            creator_id: (creator_dialog_id != DialogId::default())
                .then(|| env.get_message_sender_object(creator_dialog_id, "premiumGiftCodeInfo")),
            creation_date: result.date,
            is_from_giveaway: result.via_giveaway,
            giveaway_message_id: message_id.get(),
            month_count: if crate::message_content::get_premium_duration_day_count(month_count)
                == result.days
            {
                month_count
            } else {
                0
            },
            day_count: result.days,
            user_id: env.get_user_id_object(user_id, "premiumGiftCodeInfo"),
            use_date: result.used_date,
        });
    }

    /// `GetPremiumGiftOptionsQuery::on_result`'s promise pop
    /// (Premium.cpp:411-456).
    pub(crate) fn take_gift_options_promise(
        &mut self,
    ) -> Option<Promise<td_api::PremiumGiftPaymentOptions>> {
        if self.parked_gift_payment_options.is_empty() {
            None
        } else {
            Some(self.parked_gift_payment_options.remove(0))
        }
    }

    /// `GetPremiumGiftOptionsQuery::on_result` (Premium.cpp:411-456) —
    /// the filter (users == 1, months > 0, amount > 0, non-empty
    /// currency, store_quantity <= 1), the XTR star-price map, the
    /// per-currency max monthly price and the relative discount, sorted
    /// by month count. The sticker leg
    /// (`stickers_manager_->get_premium_gift_sticker_object`) rides the
    /// stickers plane — the None-leg convention.
    pub(crate) fn on_get_premium_gift_options(
        &mut self,
        result: Result<Vec<telegram_api::PremiumGiftCodeOption>>,
        promise: Promise<td_api::PremiumGiftPaymentOptions>,
    ) {
        let mut promise = promise;
        let results = match result {
            Result::Err(status) => return promise.set_error(status),
            Result::Ok(results) => results,
        };
        let results: Vec<_> = results
            .into_iter()
            .filter(|option| {
                option.users == 1
                    && option.months > 0
                    && option.amount > 0
                    && !option.currency.is_empty()
                    && option.store_quantity <= 1
            })
            .collect();
        let monthly_price = |option: &telegram_api::PremiumGiftCodeOption| {
            option.amount as f64 / option.months as f64
        };
        let mut star_prices: std::collections::HashMap<i32, i64> = std::collections::HashMap::new();
        let mut max_prices: std::collections::HashMap<String, f64> =
            std::collections::HashMap::new();
        for option in &results {
            if option.currency == "XTR" {
                if star_prices
                    .insert(
                        option.months,
                        crate::star_manager::get_star_count(option.amount, false),
                    )
                    .is_some()
                {
                    tdutils::log_error!(
                        "Receive duplicate Telegram Star price for {}",
                        option.months
                    );
                }
                continue;
            }
            let price = monthly_price(option);
            let max_price = max_prices.entry(option.currency.clone()).or_insert(0.0);
            if price > *max_price {
                *max_price = price;
            }
        }
        let mut options = Vec::new();
        for option in results {
            if option.currency == "XTR" {
                continue;
            }
            let relative_price = monthly_price(&option) / max_prices[&option.currency];
            options.push(td_api::PremiumGiftPaymentOption {
                currency: option.currency,
                amount: option.amount,
                star_count: star_prices.get(&option.months).copied().unwrap_or(0),
                discount_percentage: (100.0 * (1.0 - relative_price)) as i32,
                month_count: option.months,
                store_product_id: option.store_product,
                // `get_premium_gift_sticker_object` — [blocked: stickers]
                sticker: None,
            });
        }
        options.sort_by_key(|option| option.month_count);
        promise.set_value(td_api::PremiumGiftPaymentOptions { options });
    }

    /// `gift_premium_with_stars` (Premium.cpp) — the query is the seam.
    pub fn gift_premium_with_stars(
        &mut self,
        env: &mut impl PremiumEnv,
        user_id: i64,
        star_count: i64,
        month_count: i32,
        promise: Promise<()>,
    ) {
        env.send_premium_gift_query(user_id, star_count, month_count);
        self.net_blocked_promises.push(promise);
    }

    /// `launch_prepaid_premium_giveaway` (Premium.cpp:710-717) — the
    /// `GiveawayParameters::get_giveaway_parameters` validation runs the
    /// non-empty band here; the boosted-channel/option ladder is the
    /// PaymentsEnv plane (the payments arms are not Td-wired yet) and
    /// rides the seam. The query is the seam.
    pub fn launch_prepaid_premium_giveaway(
        &mut self,
        env: &mut impl PremiumEnv,
        giveaway_id: i64,
        parameters: Option<&td_api::GiveawayParameters>,
        user_count: i32,
        star_count: i64,
        mut promise: Promise<()>,
    ) {
        let Some(_parameters) = parameters else {
            return promise.set_error(Status::error(400, "Giveaway parameters must be non-empty"));
        };
        env.send_launch_prepaid_giveaway_query(giveaway_id, user_count, star_count);
        self.net_blocked_promises.push(promise);
    }

    /// `get_premium_giveaway_info` (Premium.cpp:1437-1442) →
    /// `MessagesManager::get_giveaway_message_id` (cpp:36676-36697): the
    /// `get_message_force` + giveaway-content bands ride the M.10 seam,
    /// the pure id bands run here →
    /// `GetGiveawayInfoQuery::send` (cpp:741-748): the Read input peer
    /// resolves at the send leg; the `None` arm is the handler's
    /// `on_error` (cpp:820-824 — the dialog-error tail, then `set_error`).
    pub fn get_premium_giveaway_info(
        &mut self,
        env: &mut impl PremiumEnv,
        dialog_id: i64,
        message_id: i64,
        mut promise: Promise<td_api::GiveawayInfo>,
    ) {
        let full_message_id = crate::messages_manager::MessageId::new(message_id);
        if full_message_id.is_scheduled() {
            return promise.set_error(Status::error(400, "Wrong scheduled message identifier"));
        }
        let server_message_id = if full_message_id.is_server() {
            full_message_id.get_server_message_id()
        } else {
            return promise.set_error(Status::error(400, "Wrong message identifier"));
        };
        let dialog = DialogId::new(dialog_id);
        let input_peer = match env.get_input_peer(dialog, AccessRights::Read) {
            Some(peer) => peer,
            None => {
                let status = Status::error(400, "Can't access the chat");
                env.on_get_dialog_error(dialog, &status, "GetGiveawayInfoQuery");
                return promise.set_error(status);
            }
        };
        env.send_get_giveaway_info_query(dialog_id, input_peer, server_message_id.get());
        self.parked_giveaway_infos.push(promise);
    }

    /// `GetGiveawayInfoQuery::on_result`'s promise pop
    /// (Premium.cpp:750-818).
    pub(crate) fn take_giveaway_info_promise(&mut self) -> Option<Promise<td_api::GiveawayInfo>> {
        if self.parked_giveaway_infos.is_empty() {
            None
        } else {
            Some(self.parked_giveaway_infos.remove(0))
        }
    }

    /// `GetGiveawayInfoQuery::on_result` (Premium.cpp:750-818) — the
    /// `payments_giveawayInfo` participant-status ladder and the
    /// `payments_giveawayInfoResults` winner-count clamps. The error arm
    /// is `on_error` (cpp:820-824): the dialog-error tail, then
    /// `set_error`.
    pub(crate) fn on_get_giveaway_info(
        &mut self,
        env: &mut impl PremiumEnv,
        dialog_id: i64,
        result: Result<telegram_api::payments_GiveawayInfo>,
        mut promise: Promise<td_api::GiveawayInfo>,
    ) {
        let info = match result {
            Result::Err(status) => {
                env.on_get_dialog_error(DialogId::new(dialog_id), &status, "GetGiveawayInfoQuery");
                return promise.set_error(status);
            }
            Result::Ok(info) => info,
        };
        match info {
            telegram_api::payments_GiveawayInfo::payments_giveawayInfo(info) => {
                // the participant-status ladder (cpp:757-777) — the
                // unknown-admin-disallowed-chat arm logs and falls
                // through the remaining bands
                let status = if info.joined_too_early_date > 0 {
                    Some(td_api::GiveawayParticipantStatus::giveawayParticipantStatusAlreadyWasMember(
                        td_api::giveawayParticipantStatusAlreadyWasMember {
                            joined_chat_date: info.joined_too_early_date,
                        },
                    ))
                } else if info.admin_disallowed_chat_id > 0 {
                    let channel_id =
                        crate::chat_manager::ChannelId::new(info.admin_disallowed_chat_id);
                    if !channel_id.is_valid()
                        || !env.have_channel_force(channel_id, "GetGiveawayInfoQuery")
                    {
                        tdutils::log_error!(
                            "Receive invalid admin disallowed chat {}",
                            info.admin_disallowed_chat_id
                        );
                        None
                    } else {
                        let dialog = DialogId::from_channel_id(channel_id);
                        env.force_create_dialog(dialog, "GetGiveawayInfoQuery", false);
                        Some(
                            td_api::GiveawayParticipantStatus::giveawayParticipantStatusAdministrator(
                                td_api::giveawayParticipantStatusAdministrator {
                                    chat_id: crate::dialog_manager::get_chat_id_object(
                                        dialog,
                                        "giveawayParticipantStatusAdministrator",
                                    ),
                                },
                            ),
                        )
                    }
                } else {
                    None
                };
                let status = status.unwrap_or_else(|| {
                    if !info.disallowed_country.is_empty() {
                        td_api::GiveawayParticipantStatus::giveawayParticipantStatusDisallowedCountry(
                            td_api::giveawayParticipantStatusDisallowedCountry {
                                user_country_code: info.disallowed_country.clone(),
                            },
                        )
                    } else if info.participating {
                        td_api::GiveawayParticipantStatus::giveawayParticipantStatusParticipating(
                            td_api::giveawayParticipantStatusParticipating,
                        )
                    } else {
                        td_api::GiveawayParticipantStatus::giveawayParticipantStatusEligible(
                            td_api::giveawayParticipantStatusEligible,
                        )
                    }
                });
                promise.set_value(td_api::GiveawayInfo::giveawayInfoOngoing(
                    td_api::giveawayInfoOngoing {
                        creation_date: std::cmp::max(0, info.start_date),
                        status: Some(status),
                        // the C++ passes `preparing_results` into the
                        // `is_ended` field position (the scheme reuse)
                        is_ended: info.preparing_results,
                    },
                ));
            }
            telegram_api::payments_GiveawayInfo::payments_giveawayInfoResults(info) => {
                // the winner-count clamps (cpp:795-806)
                let mut winner_count = info.winners_count;
                let mut activated_count = info.activated_count;
                if activated_count < 0 || activated_count > winner_count {
                    tdutils::log_error!(
                        "Receive invalid giveaway results: winners = {winner_count}, activated = {activated_count}"
                    );
                    if activated_count < 0 {
                        activated_count = 0;
                    }
                    if winner_count < 0 {
                        winner_count = 0;
                    }
                    if activated_count > winner_count {
                        activated_count = winner_count;
                    }
                }
                promise.set_value(td_api::GiveawayInfo::giveawayInfoCompleted(
                    td_api::giveawayInfoCompleted {
                        creation_date: std::cmp::max(0, info.start_date),
                        actual_winners_selection_date: std::cmp::max(0, info.finish_date),
                        was_refunded: info.refunded,
                        is_winner: info.winner,
                        winner_count,
                        activation_count: activated_count,
                        gift_code: info.gift_code_slug,
                        won_star_count: crate::star_manager::get_star_count(
                            info.stars_prize,
                            false,
                        ),
                    },
                ));
            }
        }
    }

    /// `can_purchase_premium` (Premium.cpp:1444-1446) →
    /// `CanPurchasePremiumQuery::send` (cpp:830-838) — the store purpose
    /// converts at the send leg; the error arm is the handler's
    /// `on_error` (plain `set_error`).
    pub fn can_purchase_premium(
        &mut self,
        env: &mut impl PremiumEnv,
        purpose: Option<td_api::StorePaymentPurpose>,
        mut promise: Promise<()>,
    ) {
        let input_purpose = match env.get_input_store_payment_purpose(purpose) {
            Result::Ok(input_purpose) => input_purpose,
            Result::Err(status) => return promise.set_error(status),
        };
        env.send_can_purchase_premium_query(input_purpose);
        self.pending_can_purchase.push(promise);
    }

    /// `CanPurchasePremiumQuery::on_result`'s promise pop
    /// (Premium.cpp:840-848).
    pub(crate) fn take_can_purchase_promise(&mut self) -> Option<Promise<()>> {
        if self.pending_can_purchase.is_empty() {
            None
        } else {
            Some(self.pending_can_purchase.remove(0))
        }
    }

    /// `assign_store_transaction` (Premium.cpp:1450-1487) — the
    /// transaction/purpose validation ladder, the PremiumSubscription
    /// suggested-action dismisses, then the
    /// `AssignAppStoreTransactionQuery`/`AssignPlayMarketTransactionQuery`
    /// sends (cpp:860-893); the conversion error arm is the handlers'
    /// `on_error` (plain `set_error`).
    pub fn assign_store_transaction(
        &mut self,
        env: &mut impl PremiumEnv,
        transaction: Option<td_api::StoreTransaction>,
        purpose: Option<td_api::StorePaymentPurpose>,
        mut promise: Promise<()>,
    ) {
        let Some(transaction) = transaction else {
            return promise.set_error(Status::error(400, "Transaction must be non-empty"));
        };
        if matches!(
            &purpose,
            Some(td_api::StorePaymentPurpose::storePaymentPurposePremiumSubscription(_))
        ) {
            use crate::suggested_action::SuggestedActionType;
            env.dismiss_suggested_action(SuggestedActionType::UpgradePremium);
            env.dismiss_suggested_action(SuggestedActionType::SubscribeToAnnualPremium);
            env.dismiss_suggested_action(SuggestedActionType::RestorePremium);
        }
        let (receipt, is_app_store) = match transaction {
            td_api::StoreTransaction::storeTransactionAppStore(t) => (t.receipt, true),
            td_api::StoreTransaction::storeTransactionGooglePlay(t) => {
                let (Some(package_name), Some(store_product_id), Some(purchase_token)) = (
                    crate::misc::clean_input_string(&t.package_name),
                    crate::misc::clean_input_string(&t.store_product_id),
                    crate::misc::clean_input_string(&t.purchase_token),
                ) else {
                    return promise
                        .set_error(Status::error(400, "Strings must be encoded in UTF-8"));
                };
                // the C++ `json_encode<string>(json_object(...))` receipt
                // (cpp:881-886) — the builder escapes the fields; the
                // seam carries it as bytes (the AppStore form)
                let mut receipt = tdutils::json::JsonBuilder::new(false);
                receipt.object(|o| {
                    o.field("packageName", package_name.as_str());
                    o.field("purchaseToken", purchase_token.as_str());
                    o.field("productId", store_product_id.as_str());
                });
                (receipt.as_str().as_bytes().to_vec(), false)
            }
        };
        let input_purpose = match env.get_input_store_payment_purpose(purpose) {
            Result::Ok(input_purpose) => input_purpose,
            Result::Err(status) => return promise.set_error(status),
        };
        env.send_assign_store_transaction_query(receipt, input_purpose, is_app_store);
        self.pending_assign_store_transaction.push(promise);
    }

    /// The store-transaction assigns' promise pop (the Updates answers
    /// re-enter `on_get_updates` with the promise).
    pub(crate) fn take_assign_store_transaction_promise(&mut self) -> Option<Promise<()>> {
        if self.pending_assign_store_transaction.is_empty() {
            None
        } else {
            Some(self.pending_assign_store_transaction.remove(0))
        }
    }
}

/// The feature-name → td_api mapping (the C++ `get_premium_feature_object`);
/// the unknown names are logged and dropped (the C++ behavior).
fn get_premium_feature_object(feature: &str) -> Option<td_api::PremiumFeature> {
    let object = match feature {
        "stories" => td_api::PremiumFeature::premiumFeatureUpgradedStories(
            td_api::premiumFeatureUpgradedStories,
        ),
        "more_upload" => td_api::PremiumFeature::premiumFeatureIncreasedUploadFileSize(
            td_api::premiumFeatureIncreasedUploadFileSize,
        ),
        "double_limits" => td_api::PremiumFeature::premiumFeatureIncreasedLimits(
            td_api::premiumFeatureIncreasedLimits,
        ),
        "business" => {
            td_api::PremiumFeature::premiumFeatureBusiness(td_api::premiumFeatureBusiness)
        }
        "last_seen" => {
            td_api::PremiumFeature::premiumFeatureLastSeenTimes(td_api::premiumFeatureLastSeenTimes)
        }
        "voice_to_text" => td_api::PremiumFeature::premiumFeatureVoiceRecognition(
            td_api::premiumFeatureVoiceRecognition,
        ),
        "faster_download" => td_api::PremiumFeature::premiumFeatureImprovedDownloadSpeed(
            td_api::premiumFeatureImprovedDownloadSpeed,
        ),
        "translations" => td_api::PremiumFeature::premiumFeatureRealTimeChatTranslation(
            td_api::premiumFeatureRealTimeChatTranslation,
        ),
        "animated_emoji" => {
            td_api::PremiumFeature::premiumFeatureCustomEmoji(td_api::premiumFeatureCustomEmoji)
        }
        "emoji_status" => {
            td_api::PremiumFeature::premiumFeatureEmojiStatus(td_api::premiumFeatureEmojiStatus)
        }
        "saved_tags" => td_api::PremiumFeature::premiumFeatureSavedMessagesTags(
            td_api::premiumFeatureSavedMessagesTags,
        ),
        "peer_colors" => {
            td_api::PremiumFeature::premiumFeatureAccentColor(td_api::premiumFeatureAccentColor)
        }
        "wallpapers" => td_api::PremiumFeature::premiumFeatureBackgroundForBoth(
            td_api::premiumFeatureBackgroundForBoth,
        ),
        "profile_badge" => {
            td_api::PremiumFeature::premiumFeatureProfileBadge(td_api::premiumFeatureProfileBadge)
        }
        "message_privacy" => td_api::PremiumFeature::premiumFeatureMessagePrivacy(
            td_api::premiumFeatureMessagePrivacy,
        ),
        "advanced_chat_management" => td_api::PremiumFeature::premiumFeatureAdvancedChatManagement(
            td_api::premiumFeatureAdvancedChatManagement,
        ),
        "no_ads" => {
            td_api::PremiumFeature::premiumFeatureDisabledAds(td_api::premiumFeatureDisabledAds)
        }
        "app_icons" => {
            td_api::PremiumFeature::premiumFeatureAppIcons(td_api::premiumFeatureAppIcons)
        }
        "infinite_reactions" => td_api::PremiumFeature::premiumFeatureUniqueReactions(
            td_api::premiumFeatureUniqueReactions,
        ),
        "animated_userpics" => td_api::PremiumFeature::premiumFeatureAnimatedProfilePhoto(
            td_api::premiumFeatureAnimatedProfilePhoto,
        ),
        "premium_stickers" => td_api::PremiumFeature::premiumFeatureUniqueStickers(
            td_api::premiumFeatureUniqueStickers,
        ),
        "effects" => td_api::PremiumFeature::premiumFeatureMessageEffects(
            td_api::premiumFeatureMessageEffects,
        ),
        "todo" => {
            td_api::PremiumFeature::premiumFeatureChecklists(td_api::premiumFeatureChecklists)
        }
        "paid_messages" => {
            td_api::PremiumFeature::premiumFeaturePaidMessages(td_api::premiumFeaturePaidMessages)
        }
        "pm_noforwards" => td_api::PremiumFeature::premiumFeatureProtectPrivateChatContent(
            td_api::premiumFeatureProtectPrivateChatContent,
        ),
        "ai_compose" => td_api::PremiumFeature::premiumFeatureTextComposition(
            td_api::premiumFeatureTextComposition,
        ),
        "rich_formatting" => {
            td_api::PremiumFeature::premiumFeatureRichMessages(td_api::premiumFeatureRichMessages)
        }
        _ => {
            tdutils::log_error!("Receive unsupported Premium feature {feature}");
            return None;
        }
    };
    Some(object)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestEnv {
        options: std::collections::BTreeMap<String, i64>,
        option_strings: std::collections::BTreeMap<String, String>,
        /// the dialogs `have_dialog_force` answers `true` for (the
        /// user-dialog seeding of the boost-ladder tests).
        dialogs: std::collections::BTreeSet<i64>,
        queries: Vec<String>,
    }

    impl PremiumEnv for TestEnv {
        fn get_option_integer(&self, name: &str) -> i64 {
            self.options.get(name).copied().unwrap_or(0)
        }

        fn get_option_string(&self, name: &str, default_value: &str) -> String {
            self.option_strings
                .get(name)
                .cloned()
                .unwrap_or_else(|| default_value.to_string())
        }

        fn send_update(&mut self, _update: td_api::Update) {}

        // the whole-Td reads — the stub-inventory posture (the unloaded
        // dialog/channel/user halves)
        fn have_dialog_force(&mut self, dialog_id: DialogId, _source: &str) -> bool {
            self.dialogs.contains(&dialog_id.get())
        }

        fn get_channel_status(&mut self, _channel_id: ChannelId) -> DialogParticipantStatus {
            DialogParticipantStatus::default()
        }

        fn get_input_peer(
            &mut self,
            _dialog_id: DialogId,
            _access_rights: AccessRights,
        ) -> Option<telegram_api::InputPeer> {
            None
        }

        fn on_get_dialog_error(&mut self, _dialog_id: DialogId, _status: &Status, _source: &str) {}

        fn on_get_users(&mut self, _users: Vec<telegram_api::User>, _source: &str) {}

        fn on_get_chats(&mut self, _chats: Vec<telegram_api::Chat>, _source: &str) {}

        fn have_channel_force(&mut self, _channel_id: ChannelId, _source: &str) -> bool {
            false
        }

        fn force_create_dialog(&mut self, _dialog_id: DialogId, _source: &str, _force_read: bool) {}

        fn get_input_user(&mut self, _user_id: UserId) -> Result<telegram_api::InputUser> {
            Result::from_status(Status::error(400, "User not found"))
        }

        fn have_dialog_info_force(&mut self, _dialog_id: DialogId, _source: &str) -> bool {
            false
        }

        fn get_message_sender_object(
            &mut self,
            dialog_id: DialogId,
            _source: &str,
        ) -> td_api::MessageSender {
            td_api::MessageSender::messageSenderChat(td_api::messageSenderChat {
                chat_id: dialog_id.get(),
            })
        }

        fn get_user_id_object(&mut self, user_id: UserId, _source: &str) -> i64 {
            user_id.get()
        }

        fn send_get_premium_promo_query(&mut self) {
            self.queries.push("getPremiumPromo".to_string());
        }

        fn send_get_premium_gift_options_query(&mut self) {
            self.queries.push("getPremiumGiftOptions".to_string());
        }

        fn send_check_gift_code_query(&mut self, code: &str) {
            self.queries.push(format!("checkGiftCode {code}"));
        }

        fn send_apply_gift_code_query(&mut self, code: &str) {
            self.queries.push(format!("applyGiftCode {code}"));
        }

        fn send_premium_gift_query(&mut self, user_id: i64, star_count: i64, month_count: i32) {
            self.queries.push(format!(
                "sendStarsGift {user_id} {star_count} {month_count}"
            ));
        }
    }

    fn env_with_limits() -> TestEnv {
        let mut options = std::collections::BTreeMap::new();
        options.insert("channels_limit_default".to_string(), 10);
        options.insert("channels_limit_premium".to_string(), 20);
        TestEnv {
            options,
            option_strings: std::collections::BTreeMap::new(),
            dialogs: std::collections::BTreeSet::new(),
            queries: Vec::new(),
        }
    }

    #[test]
    fn payment_amount_validation() {
        assert!(check_payment_amount("USD", 100).is_ok());
        assert!(check_payment_amount("USD", 0).is_error());
        assert!(check_payment_amount("USD", 999_999_999_999_999).is_error());
    }

    #[test]
    fn limits_map_keys_and_read_the_options() {
        assert_eq!(
            get_limit_type_key(&td_api::PremiumLimitType::premiumLimitTypeSupergroupCount(
                td_api::premiumLimitTypeSupergroupCount
            )),
            "channels"
        );
        let env = env_with_limits();
        let manager = PremiumManager::new();
        let (promise, recorded) = record_limit();
        manager.get_premium_limit(
            &env,
            &Some(td_api::PremiumLimitType::premiumLimitTypeSupergroupCount(
                td_api::premiumLimitTypeSupergroupCount,
            )),
            promise,
        );
        let limit = recorded.0.lock().unwrap().take().unwrap();
        assert_eq!(limit.default_value, 10);
        assert_eq!(limit.premium_value, 20);
    }

    #[test]
    fn features_filter_unknown_names() {
        let env = TestEnv {
            options: std::collections::BTreeMap::new(),
            option_strings: std::collections::BTreeMap::new(),
            dialogs: std::collections::BTreeSet::new(),
            queries: Vec::new(),
        };
        let manager = PremiumManager::new();
        let (promise, recorded) = record_features();
        manager.get_premium_features(&env, promise);
        let features = recorded.0.lock().unwrap().take().unwrap();
        assert!(!features.features.is_empty());
    }

    #[test]
    fn the_router_dispatches_through_the_seams() {
        let mut env = TestEnv {
            options: std::collections::BTreeMap::new(),
            option_strings: std::collections::BTreeMap::new(),
            dialogs: std::collections::BTreeSet::new(),
            queries: Vec::new(),
        };
        let mut manager = PremiumManager::new();
        manager.get_premium_state(&mut env, Promise::default());
        manager.get_premium_gift_payment_options(&mut env, Promise::default());
        manager.check_premium_gift_code(&mut env, "CODE", Promise::default());
        manager.apply_premium_gift_code(&mut env, "CODE", Promise::default());
        manager.gift_premium_with_stars(&mut env, 5, 500, 3, Promise::default());
        assert!(env.queries.iter().any(|q| q == "getPremiumPromo"));
        assert!(env.queries.iter().any(|q| q == "getPremiumGiftOptions"));
        assert!(env.queries.iter().any(|q| q == "checkGiftCode CODE"));
        assert!(env.queries.iter().any(|q| q == "applyGiftCode CODE"));
        assert!(env.queries.iter().any(|q| q == "sendStarsGift 5 500 3"));
        assert_eq!(
            manager.net_blocked_promises.len(),
            1,
            "the stars-gift park; apply parks its own flow, the check its own type"
        );
        assert_eq!(manager.pending_apply_gift_code.len(), 1);
        assert_eq!(manager.parked_gift_code_infos.len(), 1);
    }

    #[test]
    fn giveaway_options_resolve_the_boost_peer_at_the_send_leg() {
        let mut env = TestEnv {
            options: std::collections::BTreeMap::new(),
            option_strings: std::collections::BTreeMap::new(),
            dialogs: std::collections::BTreeSet::new(),
            queries: Vec::new(),
        };
        let mut manager = PremiumManager::new();

        // an unknown channel — the C++
        // `on_error` arm: the promise errors, nothing parks
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let capture = slot.clone();
        let promise = tdutils::promise::PromiseCreator::lambda_result(move |r| {
            *capture.lock().unwrap() = Some(r)
        });
        manager.get_premium_giveaway_options(&mut env, -1000000000000, promise);
        assert!(manager.parked_giveaway_options.is_empty());
        assert!(env.queries.is_empty());
        assert!(
            matches!(*slot.lock().unwrap(), Some(tdutils::status::Result::Err(ref status)) if status.message() == "Chat to boost not found")
        );

        // a seeded non-channel dialog fails the second band instead
        env.dialogs.insert(10);
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let capture = slot.clone();
        let promise = tdutils::promise::PromiseCreator::lambda_result(move |r| {
            *capture.lock().unwrap() = Some(r)
        });
        manager.get_premium_giveaway_options(&mut env, 10, promise);
        assert!(
            matches!(*slot.lock().unwrap(), Some(tdutils::status::Result::Err(ref status)) if status.message() == "Can't boost the chat")
        );
    }

    #[test]
    fn gift_option_helpers_work() {
        // a smoke test of the shared value class through this module
        let option = crate::premium_gift_option::PremiumGiftOption::default();
        assert!(!option.is_valid());
        let objects = crate::premium_gift_option::get_premium_state_payment_options_object(&[]);
        assert!(objects.is_empty());
    }

    struct RecordedLimit(std::sync::Arc<std::sync::Mutex<Option<td_api::PremiumLimit>>>);

    struct RecordingLimit(std::sync::Arc<std::sync::Mutex<Option<td_api::PremiumLimit>>>);

    impl tdutils::promise::PromiseInterface<td_api::PremiumLimit> for RecordingLimit {
        fn set_value(&mut self, value: td_api::PremiumLimit) {
            *self.0.lock().unwrap() = Some(value);
        }

        fn set_error(&mut self, _error: Status) {}
    }

    fn record_limit() -> (Promise<td_api::PremiumLimit>, RecordedLimit) {
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        (
            Promise::from_interface(RecordingLimit(recorded.clone())),
            RecordedLimit(recorded),
        )
    }

    struct RecordedFeatures(std::sync::Arc<std::sync::Mutex<Option<td_api::PremiumFeatures>>>);

    struct RecordingFeatures(std::sync::Arc<std::sync::Mutex<Option<td_api::PremiumFeatures>>>);

    impl tdutils::promise::PromiseInterface<td_api::PremiumFeatures> for RecordingFeatures {
        fn set_value(&mut self, value: td_api::PremiumFeatures) {
            *self.0.lock().unwrap() = Some(value);
        }

        fn set_error(&mut self, _error: Status) {}
    }

    fn record_features() -> (Promise<td_api::PremiumFeatures>, RecordedFeatures) {
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        (
            Promise::from_interface(RecordingFeatures(recorded.clone())),
            RecordedFeatures(recorded),
        )
    }
    #[test]
    fn launch_prepaid_giveaway_validates_and_parks() {
        let mut env = TestEnv {
            options: std::collections::BTreeMap::new(),
            option_strings: std::collections::BTreeMap::new(),
            dialogs: std::collections::BTreeSet::new(),
            queries: Vec::new(),
        };
        let mut manager = PremiumManager::default();

        // the C++ get_giveaway_parameters non-empty band answers locally
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let capture = slot.clone();
        let promise = tdutils::promise::PromiseCreator::lambda_result(move |r| {
            *capture.lock().unwrap() = Some(r)
        });
        manager.launch_prepaid_premium_giveaway(&mut env, 7, None, 10, 100, promise);
        assert!(
            matches!(*slot.lock().unwrap(), Some(tdutils::status::Result::Err(ref status)) if status.message() == "Giveaway parameters must be non-empty"),
            "expected the parameters error"
        );

        // the validated query parks (the parameters ladder rides the
        // PaymentsEnv plane)
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let capture = slot.clone();
        let promise = tdutils::promise::PromiseCreator::lambda_result(move |r| {
            *capture.lock().unwrap() = Some(r)
        });
        let parameters = td_api::GiveawayParameters {
            boosted_chat_id: -1000000000000,
            additional_chat_ids: Vec::new(),
            winners_selection_date: 2000000000,
            only_new_members: false,
            has_public_winners: true,
            country_codes: Vec::new(),
            prize_description: "Prize".to_string(),
        };
        manager.launch_prepaid_premium_giveaway(&mut env, 7, Some(&parameters), 10, 100, promise);
        assert_eq!(manager.net_blocked_promises.len(), 1);
        assert!((*slot.lock().unwrap()).is_none());
    }
}
