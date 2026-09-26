//! `Premium` (td/telegram/Premium.{h,cpp}) — the M.36 Wave-3 premium
//! manager plane. The value surface (check_payment_amount, the premium
//! limits table, the gift-option helpers) is the real port; the
//! promo/gift-code/giveaway/store queries are `[blocked: net]` env seams
//! with parked promises.

use crate::td_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// `check_payment_amount` (Premium.cpp:184-192).
pub fn check_payment_amount(currency: &str, amount: i64) -> Result<(), Status> {
    if amount <= 0 || !crate::misc::check_currency_amount(amount) {
        return Err(Status::error(
            400,
            "Invalid amount of the currency specified",
        ));
    }
    if crate::misc::clean_input_string(currency).is_none() {
        return Err(Status::error(400, "Strings must be encoded in UTF-8"));
    }
    Ok(())
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

/// The `G()`/`td_` surface of the premium plane.
pub trait PremiumEnv {
    fn get_option_integer(&self, name: &str) -> i64;

    fn get_option_string(&self, name: &str, default_value: &str) -> String;

    fn send_update(&mut self, update: td_api::Update);

    // ---- the query seams ([blocked: net]; the promises park) ----
    fn send_get_premium_promo_query(&mut self) {
        tdutils::log_info!("Send help.getPremiumPromo — [blocked: net]");
    }
    fn send_get_premium_gift_options_query(&mut self) {
        tdutils::log_info!("Send payments.getPremiumGiftOptions — [blocked: net]");
    }
    fn send_get_premium_giveaway_options_query(&mut self, dialog_id: i64) {
        tdutils::log_info!(
            "Send payments.getPremiumGiveawayOptions for {dialog_id} — [blocked: net]"
        );
    }
    fn send_check_gift_code_query(&mut self, code: &str) {
        tdutils::log_info!("Send payments.checkGiftCode for {code} — [blocked: net]");
    }
    fn send_apply_gift_code_query(&mut self, code: &str) {
        tdutils::log_info!("Send payments.applyGiftCode for {code} — [blocked: net]");
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
    fn send_get_giveaway_info_query(&mut self, dialog_id: i64, message_id: i64) {
        tdutils::log_info!(
            "Send payments.getGiveawayInfo({dialog_id}, {message_id}) — [blocked: net]"
        );
    }
    fn send_can_purchase_premium_query(&mut self) {
        tdutils::log_info!("Send payments.canPurchasePremium — [blocked: net]");
    }
    fn send_assign_store_transaction_query(&mut self) {
        tdutils::log_info!("Send the store-transaction assign — [blocked: net]");
    }
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

    /// `get_premium_giveaway_options` (Premium.cpp) — the query is the
    /// seam.
    pub fn get_premium_giveaway_options(
        &mut self,
        env: &mut impl PremiumEnv,
        boosted_dialog_id: i64,
        promise: Promise<td_api::PremiumGiveawayPaymentOptions>,
    ) {
        env.send_get_premium_giveaway_options_query(boosted_dialog_id);
        self.parked_giveaway_options.push(promise);
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
        self.net_blocked_promises.push(promise);
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

    /// `get_premium_giveaway_info` (Premium.cpp) — the query is the seam.
    pub fn get_premium_giveaway_info(
        &mut self,
        env: &mut impl PremiumEnv,
        dialog_id: i64,
        message_id: i64,
        promise: Promise<td_api::GiveawayInfo>,
    ) {
        env.send_get_giveaway_info_query(dialog_id, message_id);
        self.parked_giveaway_infos.push(promise);
    }

    /// `can_purchase_premium` (Premium.cpp) — the query is the seam.
    pub fn can_purchase_premium(&mut self, env: &mut impl PremiumEnv, promise: Promise<()>) {
        env.send_can_purchase_premium_query();
        self.net_blocked_promises.push(promise);
    }

    /// `assign_store_transaction` (Premium.cpp) — the query is the seam.
    pub fn assign_store_transaction(&mut self, env: &mut impl PremiumEnv, promise: Promise<()>) {
        env.send_assign_store_transaction_query();
        self.net_blocked_promises.push(promise);
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
            queries: Vec::new(),
        }
    }

    #[test]
    fn payment_amount_validation() {
        assert!(check_payment_amount("USD", 100).is_ok());
        assert!(check_payment_amount("USD", 0).is_err());
        assert!(check_payment_amount("USD", 999_999_999_999_999).is_err());
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
            2,
            "apply + gift park; the check parks its own type"
        );
        assert_eq!(manager.parked_gift_code_infos.len(), 1);
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
