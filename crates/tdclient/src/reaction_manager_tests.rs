//! The offline [`ReactionManagerEnv`] stub and the M.16.3 unit tests: the
//! pmc/options state lives in plain maps, every blocked net query is
//! recorded by name with its promise dropped (the unanswered-request
//! convention), and the FileId codec delegates to the real
//! [`crate::files::FileManager`], the same split the td.rs wiring uses.

use super::*;
use crate::reaction_list_type::ReactionListType;
use tdutils::promise::{Promise, PromiseInterface};

/// A [`PromiseInterface`] that flips flags instead of answering requests —
/// the offline stand-in for the deferred outbox.
struct FlagPromise<T: Send + 'static> {
    value_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    error_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _type: std::marker::PhantomData<T>,
}

impl<T: Send + 'static> PromiseInterface<T> for FlagPromise<T> {
    fn set_value(&mut self, _value: T) {
        self.value_flag
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn set_error(&mut self, _error: tdutils::status::Status) {
        self.error_flag
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[derive(Clone)]
struct FulfilledFlag {
    value_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    error_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl FulfilledFlag {
    fn new() -> (
        FulfilledFlag,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let value_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let error_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        (
            FulfilledFlag {
                value_flag: value_flag.clone(),
                error_flag: error_flag.clone(),
            },
            value_flag,
            error_flag,
        )
    }

    fn fulfilled(&self) -> bool {
        self.value_flag.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn failed(&self) -> bool {
        self.error_flag.load(std::sync::atomic::Ordering::SeqCst)
    }
}

fn parked_promise() -> (Promise<()>, FulfilledFlag) {
    let (flag, value, error) = FulfilledFlag::new();
    (
        Promise::from_interface(FlagPromise::<()> {
            value_flag: value,
            error_flag: error,
            _type: std::marker::PhantomData,
        }),
        flag,
    )
}

fn parked_tags_promise() -> (Promise<td_api::SavedMessagesTags>, FulfilledFlag) {
    let (flag, value, error) = FulfilledFlag::new();
    (
        Promise::from_interface(FlagPromise::<td_api::SavedMessagesTags> {
            value_flag: value,
            error_flag: error,
            _type: std::marker::PhantomData,
        }),
        flag,
    )
}

struct TestEnv {
    files: crate::files::FileManager,
    pmc: std::collections::HashMap<String, Vec<u8>>,
    sqlite: std::collections::HashMap<Vec<u8>, Vec<u8>>,
    options: std::collections::HashMap<String, String>,
    updates: Vec<td_api::Update>,
    queries: Vec<String>,
    authorized: bool,
    is_bot: bool,
}

impl Default for TestEnv {
    fn default() -> TestEnv {
        TestEnv {
            files: crate::files::FileManager::new(),
            pmc: std::collections::HashMap::new(),
            sqlite: std::collections::HashMap::new(),
            options: std::collections::HashMap::new(),
            updates: Vec::new(),
            queries: Vec::new(),
            authorized: true,
            is_bot: false,
        }
    }
}

impl TestEnv {
    fn pmc_blob(&self, key: &str) -> Vec<u8> {
        self.pmc.get(key).cloned().unwrap_or_default()
    }

    fn has_query(&self, prefix: &str) -> bool {
        self.queries.iter().any(|query| query.starts_with(prefix))
    }
}

impl crate::photo::DialogPhotoStoreEnv for TestEnv {
    /// `FileManager::store_file` — the real codec.
    fn store_file_id(&mut self, file_id: crate::files::FileId, out: &mut Vec<u8>) {
        self.files.store_file(file_id, out);
    }

    /// `FileManager::parse_file` — the real codec.
    fn parse_file_id(
        &mut self,
        parser: &mut Parser<'_>,
    ) -> Result<crate::files::FileId, &'static str> {
        self.files.parse_file(parser)
    }
}

impl crate::paid_reaction_type::PaidReactionTypeEnv for TestEnv {
    fn have_dialog_info(&self, _dialog_id: crate::dialog_id::DialogId) -> bool {
        false
    }

    fn force_create_dialog(&mut self, _dialog_id: crate::dialog_id::DialogId) {}

    fn check_dialog_access(
        &self,
        _dialog_id: crate::dialog_id::DialogId,
        _source: &str,
    ) -> Result<(), tdutils::status::Status> {
        Err(tdutils::status::Status::error(
            400,
            "No dialog plane in the fixture",
        ))
    }

    fn is_broadcast_channel(&self, _dialog_id: crate::dialog_id::DialogId) -> bool {
        false
    }

    fn get_input_peer(
        &self,
        _dialog_id: crate::dialog_id::DialogId,
    ) -> Option<tdmtproto::telegram_api::InputPeer> {
        None
    }

    fn get_chat_id_object(&self, _dialog_id: crate::dialog_id::DialogId, _source: &str) -> i64 {
        0
    }
}

impl ReactionManagerEnv for TestEnv {
    fn auth_is_authorized(&self) -> bool {
        self.authorized
    }

    fn auth_is_bot(&self) -> bool {
        self.is_bot
    }

    fn close_flag(&self) -> bool {
        false
    }

    fn get_option_boolean(&self, name: &str, default_value: bool) -> bool {
        match self.options.get(name).map(|value| value.as_str()) {
            Some("1") => true,
            Some("0") => false,
            _ => default_value,
        }
    }

    fn get_option_integer(&self, _name: &str, default_value: i64) -> i64 {
        default_value
    }

    fn get_option_string(&self, name: &str, default_value: &str) -> String {
        self.options
            .get(name)
            .cloned()
            .unwrap_or_else(|| default_value.to_owned())
    }

    fn have_option(&self, name: &str) -> bool {
        self.options.contains_key(name)
    }

    fn set_option_string(&mut self, name: &str, value: &str) {
        self.options.insert(name.to_owned(), value.to_owned());
    }

    fn set_option_boolean(&mut self, name: &str, value: bool) {
        self.options
            .insert(name.to_owned(), if value { "1" } else { "0" }.to_owned());
    }

    fn set_option_empty(&mut self, name: &str) {
        self.options.remove(name);
    }

    fn binlog_pmc_get_raw(&mut self, key: &str) -> Vec<u8> {
        self.pmc.get(key).cloned().unwrap_or_default()
    }

    fn binlog_pmc_set_raw(&mut self, key: &str, value: &[u8]) {
        self.pmc.insert(key.to_owned(), value.to_owned());
    }

    fn sqlite_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
        self.sqlite.get(key).cloned().unwrap_or_default()
    }

    fn sqlite_pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
        self.sqlite.insert(key.to_owned(), value.to_owned());
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }

    fn set_active_reactions(&mut self, _reactions: &[ReactionType]) {
        // the M.10 consumer; nothing to record on this line
    }

    fn get_sticker_object(&self, _file_id: crate::files::FileId) -> Option<td_api::Sticker> {
        // the M.15 seam
        None
    }

    fn send_get_available_reactions_query(&mut self, hash: i32) {
        self.queries
            .push(format!("getAvailableReactions hash={hash}"));
    }

    fn send_get_reaction_list_query(&mut self, reaction_list_type: ReactionListType, hash: i64) {
        self.queries.push(format!(
            "getReactionList {reaction_list_type:?} hash={hash}"
        ));
    }

    fn send_clear_recent_reactions_query(&mut self, _promise: Promise<()>) {
        self.queries.push("clearRecentReactions".to_owned());
    }

    fn send_set_default_reaction_query(&mut self, reaction_type: &ReactionType) {
        self.queries
            .push(format!("setDefaultReaction {}", reaction_type.get_string()));
    }

    fn send_get_saved_reaction_tags_query(&mut self, topic_id: SavedMessagesTopicId, hash: i64) {
        self.queries.push(format!(
            "getSavedReactionTags topic={} hash={hash}",
            topic_id.get_unique_id()
        ));
    }

    fn send_update_saved_reaction_tag_query(&mut self, reaction_type: &ReactionType, title: &str) {
        self.queries.push(format!(
            "updateSavedReactionTag {} {title}",
            reaction_type.get_string()
        ));
    }

    fn send_get_available_effects_query(&mut self, hash: i32) {
        self.queries
            .push(format!("getAvailableEffects hash={hash}"));
    }
}

fn emoji(reaction: &str) -> ReactionType {
    ReactionType::new(reaction.to_owned())
}

/// A real registered remote document, so the FileId codec encodes the
/// location inline and the pmc round trip reconstructs a valid id.
fn register_icon(env: &mut TestEnv, document_id: i64, name: &str) -> crate::files::FileId {
    env.files.register_remote(
        crate::files::FullRemoteFileLocation::from_document(
            crate::files::FileType::Document,
            document_id,
            1,
            crate::chat_manager::DcId::create(2),
            String::new(),
        ),
        crate::files::FileLocationSource::FromBinlog,
        crate::message_id::DialogId::default(),
        100,
        100,
        name.to_owned(),
    )
}

#[test]
fn init_on_an_empty_database_parks_the_reloads() {
    let mut env = TestEnv::default();
    let mut manager = ReactionManager::new();

    manager.init(&mut env);
    // active reactions are empty -> reload -> the blocked query
    assert!(env.has_query("getAvailableReactions"));
    assert!(env.has_query("getAvailableEffects"));
    assert!(manager.reactions.are_being_reloaded);

    // a second init is the C++ is_inited_ no-op
    let query_count = env.queries.len();
    manager.init(&mut env);
    assert_eq!(env.queries.len(), query_count);

    // bots and the unauthorized state never init
    let mut bot_env = TestEnv {
        is_bot: true,
        ..TestEnv::default()
    };
    let mut bot_manager = ReactionManager::new();
    bot_manager.init(&mut bot_env);
    assert!(!bot_manager.is_inited);
    assert!(bot_env.queries.is_empty());
}

#[test]
fn on_get_reaction_list_intake_saves_and_folds_the_hash() {
    let mut env = TestEnv::default();
    let mut manager = ReactionManager::new();
    manager.init(&mut env); // parks the available-reactions reload
    manager.load_reaction_list(&mut env, ReactionListType::Recent); // parks the Recent reload

    let reaction_types = vec![emoji("👍"), emoji("🔥")];
    let hash = get_reaction_types_hash(&reaction_types);
    manager.on_get_reaction_list(
        &mut env,
        ReactionListType::Recent,
        Some(telegram_api::messages_Reactions::messages_reactions(
            telegram_api::messages_reactions {
                hash,
                reactions: vec![
                    telegram_api::Reaction::reactionEmoji(telegram_api::reactionEmoji {
                        emoticon: "👍".to_owned(),
                    }),
                    telegram_api::Reaction::reactionEmoji(telegram_api::reactionEmoji {
                        emoticon: "🔥".to_owned(),
                    }),
                ],
            },
        )),
    );

    let list = manager.get_reaction_list(ReactionListType::Recent);
    assert_eq!(list.reaction_types, reaction_types);
    assert_eq!(list.hash, hash);
    assert!(!list.is_being_reloaded);
    assert!(list.is_loaded_from_database);
    // the list landed in the binlog pmc
    assert!(!env.pmc_blob("recent_reactions").is_empty());

    // the NotModified shape: no save. The intake runs once per reload,
    // so re-arm like the C++ reload callers do
    let update_count = env.updates.len();
    manager.reload_reaction_list(&mut env, ReactionListType::Recent);
    manager.on_get_reaction_list(
        &mut env,
        ReactionListType::Recent,
        Some(
            telegram_api::messages_Reactions::messages_reactionsNotModified(
                telegram_api::messages_reactionsNotModified {},
            ),
        ),
    );
    manager.reload_reaction_list(&mut env, ReactionListType::Recent);
    manager.on_get_reaction_list(&mut env, ReactionListType::Recent, None);
    assert_eq!(env.updates.len(), update_count);
}

#[test]
fn add_recent_reaction_and_clear_follow_the_cpp() {
    let mut env = TestEnv::default();
    let mut manager = ReactionManager::new();
    manager.init(&mut env);
    manager.load_reaction_list(&mut env, ReactionListType::Recent);
    manager.on_get_reaction_list(
        &mut env,
        ReactionListType::Recent,
        Some(telegram_api::messages_Reactions::messages_reactions(
            telegram_api::messages_reactions {
                hash: 0,
                reactions: vec![telegram_api::Reaction::reactionEmoji(
                    telegram_api::reactionEmoji {
                        emoticon: "👍".to_owned(),
                    },
                )],
            },
        )),
    );

    // a new top reaction lands at the front with the folded hash
    manager.add_recent_reaction(&mut env, &emoji("🔥"));
    let (types, hash) = {
        let list = manager.get_reaction_list(ReactionListType::Recent);
        (list.reaction_types.clone(), list.hash)
    };
    assert_eq!(types, vec![emoji("🔥"), emoji("👍")]);
    assert_eq!(hash, get_reaction_types_hash(&types));

    // re-adding the top reaction is the no-op
    manager.add_recent_reaction(&mut env, &emoji("🔥"));
    let list = manager.get_reaction_list(ReactionListType::Recent);
    assert_eq!(list.reaction_types, vec![emoji("🔥"), emoji("👍")]);

    // clearing a non-empty list clears locally and sends the blocked
    // query, keeping the request promise unanswered
    let (promise, _receiver) = parked_promise();
    manager.clear_recent_reactions(&mut env, promise);
    assert!(env.has_query("clearRecentReactions"));
    assert!(
        manager
            .get_reaction_list(ReactionListType::Recent)
            .reaction_types
            .is_empty()
    );

    // clearing the now-empty list answers immediately
    let (promise, receiver) = parked_promise();
    manager.clear_recent_reactions(&mut env, promise);
    assert!(receiver.fulfilled());
}

#[test]
fn set_default_reaction_validates_then_rides_the_options() {
    let mut env = TestEnv::default();
    let mut manager = ReactionManager::new();
    manager.init(&mut env);

    // the validation band
    let cases = [
        ReactionType::default(),
        ReactionType::paid(),
        emoji("👍"), // not active: no reactions loaded
    ];
    for reaction_type in cases {
        let (promise, receiver) = parked_promise();
        manager.set_default_reaction(&mut env, reaction_type, promise);
        assert!(!receiver.fulfilled());
        assert!(!env.has_query("setDefaultReaction"));
    }

    // an active custom reaction answers ok and drives the option machine
    manager.active_reaction_types.push(emoji("👍"));
    let (promise, receiver) = parked_promise();
    manager.set_default_reaction(&mut env, emoji("👍"), promise);
    assert!(receiver.fulfilled());
    assert_eq!(env.get_option_string("default_reaction", "-"), "👍");
    assert!(env.get_option_boolean("default_reaction_needs_sync", false));
    assert!(env.has_query("setDefaultReaction 👍"));

    // the same reaction again skips the query
    let query_count = env.queries.len();
    let (promise, receiver) = parked_promise();
    manager.set_default_reaction(&mut env, emoji("👍"), promise);
    assert!(receiver.fulfilled());
    assert_eq!(env.queries.len(), query_count);

    // a fresh manager replays the pending sync from the option
    let mut env2 = TestEnv {
        options: env.options.clone(),
        ..TestEnv::default()
    };
    let mut manager2 = ReactionManager::new();
    manager2.init(&mut env2);
    assert!(env2.has_query("setDefaultReaction 👍"));
}

#[test]
fn available_reactions_build_sweeps_every_available_type() {
    let mut env = TestEnv::default();
    let mut manager = ReactionManager::new();

    // seed the pmc "reactions" record the load parses: two active rows
    // over registered remote documents (is_valid needs live FileIds)
    let mut seed = Reactions::default();
    seed.reactions.push(Reaction {
        reaction_type: emoji("👍"),
        title: "Like".to_owned(),
        is_active: true,
        static_icon: register_icon(&mut env, 111, "static111"),
        appear_animation: register_icon(&mut env, 112, "appear112"),
        select_animation: register_icon(&mut env, 113, "select113"),
        activate_animation: register_icon(&mut env, 114, "activate114"),
        effect_animation: register_icon(&mut env, 115, "effect115"),
        ..Reaction::default()
    });
    seed.reactions.push(Reaction {
        reaction_type: emoji("🔥"),
        title: "Fire".to_owned(),
        is_active: true,
        static_icon: register_icon(&mut env, 121, "static121"),
        appear_animation: register_icon(&mut env, 122, "appear122"),
        select_animation: register_icon(&mut env, 123, "select123"),
        activate_animation: register_icon(&mut env, 124, "activate124"),
        effect_animation: register_icon(&mut env, 125, "effect125"),
        ..Reaction::default()
    });
    let mut blob = Vec::new();
    store_log_event_version(&mut blob);
    seed.store(&mut env, &mut blob);
    env.pmc.insert("reactions".to_owned(), blob);

    // the load happens on the first consumer (get_emoji_reaction et al)
    manager.load_reactions(&mut env);
    assert_eq!(manager.active_reaction_types.len(), 2);

    // the getStoryAvailableReactions path: load + build in one call
    let available = manager.get_available_reactions(&mut env, 0);
    assert_eq!(
        available.top_reactions.len() + available.popular_reactions.len(),
        2
    );
    assert!(available.allow_custom_emoji);
    assert!(available.unavailability_reason.is_none());
    assert!(available.recent_reactions.is_empty());

    // the active reactions machine ran: the update went out and the
    // option-less active list answered
    assert!(!manager.active_reaction_types.is_empty());
    assert!(manager.is_active_reaction(&emoji("👍")));

    // row_size outside 5..=25 collapses to 8 inside the builder; the
    // 2-row result cannot distinguish it, so pin the clamped size through
    // a 30-reaction list instead of asserting internals
}

#[test]
fn saved_tags_machine_round_trips_through_the_sqlite_pmc() {
    let mut env = TestEnv::default();
    let mut manager = ReactionManager::new();
    manager.init(&mut env);

    // the request path parks a promise and sends the (blocked) query
    let (promise, receiver) = parked_tags_promise();
    manager.get_saved_messages_tags(&mut env, SavedMessagesTopicId::default(), promise);
    assert!(env.has_query("getSavedReactionTags topic=0"));
    assert!(!receiver.fulfilled());

    // the wire hash folds like the C++ calc_hash
    let mut expected = SavedReactionTags::default();
    expected
        .tags
        .push(SavedReactionTag::new(&emoji("👍"), "", 2));
    let wire_hash = expected.calc_hash();

    // the tags arrive from the wire intake and complete the parked promise
    manager.on_get_saved_messages_tags(
        &mut env,
        SavedMessagesTopicId::default(),
        Ok(
            telegram_api::messages_SavedReactionTags::messages_savedReactionTags(
                telegram_api::messages_savedReactionTags {
                    tags: vec![telegram_api::SavedReactionTag {
                        flags: 0,
                        reaction: Some(telegram_api::Reaction::reactionEmoji(
                            telegram_api::reactionEmoji {
                                emoticon: "👍".to_owned(),
                            },
                        )),
                        title: String::new(),
                        count: 2,
                    }],
                    hash: wire_hash,
                },
            ),
        ),
    );
    assert!(receiver.fulfilled());
    assert!(manager.all_tags.is_inited);
    assert_eq!(manager.all_tags.tags.len(), 1);
    // the update went out with the default topic id 0
    let has_tags_update = env
        .updates
        .iter()
        .any(|update| matches!(update, td_api::Update::updateSavedMessagesTags(_)));
    assert!(has_tags_update);

    // a title set lands in the sqlite pmc and re-sends the update
    let update_count = env.updates.len();
    let (promise, receiver) = parked_promise();
    manager.set_saved_messages_tag_title(&mut env, emoji("👍"), "Great", promise);
    assert!(env.has_query("updateSavedReactionTag"));
    assert_eq!(manager.all_tags.tags[0].title, "Great");
    assert!(env.updates.len() > update_count);
    // the request promise itself parks behind the blocked reload
    assert!(!receiver.fulfilled());

    // the sqlite pmc carries the blob
    let key = b"saved_messages_tags0".to_vec();
    assert!(!env.sqlite.get(&key).unwrap().is_empty());

    // a fresh manager reloads the tags from the database and answers
    let mut env2 = TestEnv {
        sqlite: env.sqlite.clone(),
        ..TestEnv::default()
    };
    let mut manager2 = ReactionManager::new();
    manager2.init(&mut env2);
    // the all-tags record loads lazily, on the first access
    let (promise, receiver2) = parked_tags_promise();
    manager2.get_saved_messages_tags(&mut env2, SavedMessagesTopicId::default(), promise);
    assert!(receiver2.fulfilled());
    assert!(manager2.all_tags.is_inited);
    assert_eq!(manager2.all_tags.tags[0].title, "Great");
    assert!(
        env2.updates
            .iter()
            .any(|update| matches!(update, td_api::Update::updateSavedMessagesTags(_)))
    );

    // get_saved_messages_tags answers in-place once inited
    let (promise, receiver) = parked_tags_promise();
    manager2.get_saved_messages_tags(&mut env2, SavedMessagesTopicId::default(), promise);
    assert!(receiver.fulfilled());

    // a non-zero topic id has no SavedMessagesManager store on this
    // line: the C++ invalid-marker path answers 400
    let (promise, receiver) = parked_tags_promise();
    manager2.get_saved_messages_tags(
        &mut env2,
        SavedMessagesTopicId::from_dialog_id(crate::dialog_id::DialogId::new(i64::MAX)),
        promise,
    );
    assert!(!receiver.fulfilled());
    assert!(receiver.failed());
    // the rejected topic never reaches the reload query
    assert!(!env2.has_query("topic=9223372036854775807"));
}

#[test]
fn default_paid_reaction_type_persists_and_migrates() {
    let mut env = TestEnv::default();
    let mut manager = ReactionManager::new();
    manager.init(&mut env);
    // the init emitted the default-type update
    assert!(
        env.updates
            .iter()
            .any(|update| matches!(update, td_api::Update::updateDefaultPaidReactionType(_)))
    );

    // a dialog-type update persists
    manager.on_update_default_paid_reaction_type(
        &mut env,
        &PaidReactionType::dialog(crate::dialog_id::DialogId::new(-1001234567890)),
    );
    assert!(!env.pmc_blob("default_paid_reaction_type").is_empty());
    let update_count = env.updates.len();

    // the same type again is the no-op
    manager.on_update_default_paid_reaction_type(
        &mut env,
        &PaidReactionType::dialog(crate::dialog_id::DialogId::new(-1001234567890)),
    );
    assert_eq!(env.updates.len(), update_count);

    // a fresh manager parses the record back
    let mut env2 = TestEnv {
        pmc: env.pmc.clone(),
        ..TestEnv::default()
    };
    let mut manager2 = ReactionManager::new();
    manager2.load_default_paid_reaction_type(&mut env2);
    assert_eq!(
        manager2.get_default_paid_reaction_type(),
        PaidReactionType::dialog(crate::dialog_id::DialogId::new(-1001234567890))
    );

    // the legacy option migration
    let mut env3 = TestEnv::default();
    env3.options
        .insert("is_paid_reaction_anonymous".to_owned(), "1".to_owned());
    let mut manager3 = ReactionManager::new();
    manager3.load_default_paid_reaction_type(&mut env3);
    assert_eq!(
        manager3.get_default_paid_reaction_type(),
        PaidReactionType::legacy(true)
    );
    assert!(!env3.have_option("is_paid_reaction_anonymous"));
}

#[test]
fn message_effects_load_and_answer_through_the_seam() {
    let mut env = TestEnv::default();
    let mut manager = ReactionManager::new();

    // one sticker effect + one reaction effect, stored through the real
    // FileId codec over registered remote documents
    let static_icon = register_icon(&mut env, 101, "icon101");
    let sticker = register_icon(&mut env, 102, "sticker102");
    let animation = register_icon(&mut env, 105, "animation105");
    let mut seed = Effects::default();
    seed.effects.push(Effect {
        id: MessageEffectId::new(1001),
        emoji: "🔥".to_owned(),
        static_icon_id: static_icon,
        effect_sticker_id: sticker,
        effect_animation_id: crate::files::FileId::default(),
        is_premium: false,
    });
    seed.effects.push(Effect {
        id: MessageEffectId::new(1002),
        emoji: "👍".to_owned(),
        static_icon_id: register_icon(&mut env, 103, "icon103"),
        effect_sticker_id: register_icon(&mut env, 104, "sticker104"),
        effect_animation_id: animation,
        is_premium: true,
    });
    let mut blob = Vec::new();
    store_log_event_version(&mut blob);
    seed.store(&mut env, &mut blob);
    env.pmc.insert("message_effects".to_owned(), blob);

    manager.load_message_effects(&mut env);
    assert_eq!(manager.message_effects.effects.len(), 2);
    // the sticker effect sorts before the reaction effect in the active
    // split
    assert_eq!(manager.active_message_effects.sticker_effects, vec![1001]);
    assert_eq!(manager.active_message_effects.reaction_effects, vec![1002]);
    assert!(
        env.updates
            .iter()
            .any(|update| matches!(update, td_api::Update::updateAvailableMessageEffects(_)))
    );

    // the object build rides the M.15 sticker seam (null stickers here)
    let effect = manager
        .get_message_effect_by_id(&env, MessageEffectId::new(1002))
        .unwrap();
    assert_eq!(effect.id, 1002);
    assert!(effect.is_premium);
    assert!(effect.static_icon.is_none());

    // an unknown effect id answers null
    assert!(
        manager
            .get_message_effect_by_id(&env, MessageEffectId::new(404))
            .is_none()
    );

    // the active effects record persists
    assert!(!env.pmc_blob("active_message_effects").is_empty());
}

#[test]
fn get_current_state_blocks_bots_and_emits_the_updates() {
    let mut env = TestEnv::default();
    let mut manager = ReactionManager::new();
    manager.init(&mut env);

    let mut updates = Vec::new();
    manager.get_current_state(&env, &mut updates);
    // the default paid reaction type update always emits
    assert!(
        updates
            .iter()
            .any(|update| matches!(update, td_api::Update::updateDefaultPaidReactionType(_)))
    );

    let bot_env = TestEnv {
        is_bot: true,
        ..TestEnv::default()
    };
    let mut updates = Vec::new();
    manager.get_current_state(&bot_env, &mut updates);
    assert!(updates.is_empty());
}
