//! Port of `td/telegram/TranslationManager.{h,cpp}` (M.25) plus the
//! model files that only serve it: `AiComposeTone.{h,cpp,hpp}` and
//! `AiComposeToneExample.{h,cpp}`.
//!
//! The manager is the collapsed actor: `Td` owns the struct, the
//! [`TranslationManagerEnv`] trait carries the `G()`/auth/pmc/user
//! surfaces, and the ten net queries ride the client-side dispatcher
//! under their `crate::td::TranslationNetKind` — the answers drain
//! through `Td::on_translation_net_answer` into the landed
//! `on_get_ai_compose_tones`/`on_get_translated_texts` intakes, the
//! formatted-text/fixed-text answers (the proofread diff rides the
//! [`crate::diff_text`] port), and the tone CRUD style conversions;
//! the reload chain (`aicompose.getTones`) resolves the parked
//! tone-style promises (the C++ `reload_ai_compose_tones` lambda
//! chain, TranslationManager.cpp:225-227/261-263). The two C++
//! null-value tails (`GetToneQuery`'s not-modified/wrong-size bands,
//! the empty tone example) answer 500 — the port's request promises
//! are not nullable.
//!
//! Landed here: the `ai_compose_styles` pmc state (the
//! [`AiComposeTones`] value plane with the byte-exact store/parse
//! codec, the constructor restore with the dependency reset, and the
//! `updateTextCompositionStyles` out machine), the tone family
//! (create/edit/delete/search/example/add/remove over the pmc-backed
//! style lookup), `check_tone`, `clone_input_ai_compose_tone`, the
//! text intake (`get_input_text`, the M.8 entity scan), and the
//! `translateText`/`composeTextWithAi`/`fixTextWithAi` request
//! plumbing.
//!
//! Env seams (the C++ callers that do not exist yet):
//! - the message-translate form of `TranslateTextQuery` (the PEER-flag
//!   wire shape) rides with the messages plane — the landed text form
//!   covers the `translateText` request family.
//! - the rich-message half (`translateRichMessage`,
//!   `composeRichMessageWithAi`, `createRichMessageWithAi`,
//!   `fixRichMessageWithAi`) — [blocked: RichMessage]; the
//!   `RichMessage::get_rich_message` input conversion and the
//!   `get_rich_message_object` answer builders are the un-landed
//!   value plane, so the whole rich intake parks at the seam exactly
//!   like the C++ offline. The C++ validates the rich input before
//!   the tone; the port runs the tone check first (the only
//!   reachable offline band), which is a documented divergence.
//! - `UserManager::get_me` / `on_get_users` / `get_user_id_object` /
//!   the `Dependencies::resolve_force` band over the tone author ids
//!   — routed to the real [`crate::user_manager::UserManager`] by
//!   the `Td` env.

use crate::message_entity::{
    FormattedText, fix_formatted_text, get_formatted_text, get_formatted_text_object,
    get_message_entities,
};
use crate::store_parse::{Parser, store_i32, store_i64, store_string};
use crate::td_api;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::{Result, Status};

/// `TranslationManager::get_ai_compose_tones_key`
/// (TranslationManager.cpp:827-829).
pub const AI_COMPOSE_TONES_KEY: &[u8] = b"ai_compose_styles";

// ---------------------------------------------------------------------------
// The AiComposeTone value plane (AiComposeTone.h + AiComposeToneExample.h).
// ---------------------------------------------------------------------------

/// `AiComposeTone::Type` (AiComposeTone.h:24).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AiComposeToneType {
    #[default]
    Default,
    Custom,
}

/// `td::AiComposeToneExample` (AiComposeToneExample.h:17-39) — a
/// from/to `FormattedText` pair.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AiComposeToneExample {
    from_text: FormattedText,
    to_text: FormattedText,
}

impl AiComposeToneExample {
    /// `AiComposeToneExample::AiComposeToneExample`
    /// (AiComposeToneExample.cpp:21-28) — `nullptr` stays the default;
    /// the texts parse with `skip_media_timestamps = true`,
    /// `skip_trim = false`.
    pub fn new_from_telegram(example: Option<telegram_api::AiComposeToneExample>) -> Self {
        let Some(example) = example else {
            return Self::default();
        };
        let from = example.r#from.unwrap_or_default();
        let to = example.r#to.unwrap_or_default();
        let from_text = get_formatted_text(
            None::<&mut ()>,
            from.text,
            from.entities,
            true,
            false,
            "AiComposeToneExample from",
        );
        let to_text = get_formatted_text(
            None::<&mut ()>,
            to.text,
            to.entities,
            true,
            false,
            "AiComposeToneExample to",
        );
        AiComposeToneExample { from_text, to_text }
    }

    /// `AiComposeToneExample::is_empty` (AiComposeToneExample.h:28-30).
    pub fn is_empty(&self) -> bool {
        self.from_text.text.is_empty() && self.to_text.text.is_empty()
    }

    /// `AiComposeToneExample::get_text_composition_style_example_object`
    /// (AiComposeToneExample.cpp:30-37) — `None` is the C++ `nullptr`
    /// for the empty example.
    pub fn get_text_composition_style_example_object(
        &self,
    ) -> Option<td_api::TextCompositionStyleExample> {
        if self.is_empty() {
            return None;
        }
        Some(td_api::TextCompositionStyleExample {
            source_text: Some(get_formatted_text_object(
                None::<&mut ()>,
                &self.from_text,
                true,
                -1,
            )),
            result_text: Some(get_formatted_text_object(
                None::<&mut ()>,
                &self.to_text,
                true,
                -1,
            )),
        })
    }

    /// `AiComposeToneExample::store` (AiComposeToneExample.hpp) — the
    /// two `FormattedText`s back to back.
    pub fn store(&self, out: &mut Vec<u8>) {
        self.from_text.store(out);
        self.to_text.store(out);
    }

    /// `AiComposeToneExample::parse`.
    pub fn parse(input: &mut Parser<'_>) -> std::result::Result<Self, &'static str> {
        let from_text = FormattedText::parse(input)?;
        let to_text = FormattedText::parse(input)?;
        Ok(AiComposeToneExample { from_text, to_text })
    }
}

/// `td::AiComposeTone` (AiComposeTone.h:23-60).
#[derive(Clone, Debug, Default)]
pub struct AiComposeTone {
    tone_type: AiComposeToneType,
    slug: String,
    custom_emoji_id: crate::emoji_status::CustomEmojiId,
    title: String,

    is_creator: bool,
    id: i64,
    access_hash: i64,
    install_count: i32,
    prompt: String,
    author_user_id: crate::user_manager::UserId,
    english_example: AiComposeToneExample,
}

impl PartialEq for AiComposeTone {
    /// `operator==(const AiComposeTone &, const AiComposeTone &)`
    /// (AiComposeTone.cpp:81-87).
    fn eq(&self, other: &Self) -> bool {
        self.tone_type == other.tone_type
            && self.slug == other.slug
            && self.custom_emoji_id == other.custom_emoji_id
            && self.title == other.title
            && self.is_creator == other.is_creator
            && self.id == other.id
            && self.access_hash == other.access_hash
            && self.install_count == other.install_count
            && self.prompt == other.prompt
            && self.author_user_id == other.author_user_id
            && self.english_example == other.english_example
    }
}

impl AiComposeTone {
    /// `AiComposeTone::AiComposeTone(telegram_api::object_ptr<...>)`
    /// (AiComposeTone.cpp:19-56).
    pub fn new_from_telegram(tone: telegram_api::AiComposeTone) -> Self {
        let mut result = match tone {
            telegram_api::AiComposeTone::aiComposeTone(tone) => AiComposeTone {
                tone_type: AiComposeToneType::Custom,
                slug: tone.slug,
                custom_emoji_id: crate::emoji_status::CustomEmojiId::new(tone.emoji_id),
                title: tone.title,
                is_creator: tone.creator,
                id: tone.id,
                access_hash: tone.access_hash,
                install_count: tone.installs_count,
                prompt: tone.prompt,
                author_user_id: crate::user_manager::UserId::new(tone.author_id),
                english_example: AiComposeToneExample::new_from_telegram(tone.example_english),
            },
            telegram_api::AiComposeTone::aiComposeToneDefault(tone) => AiComposeTone {
                tone_type: AiComposeToneType::Default,
                slug: tone.tone,
                custom_emoji_id: crate::emoji_status::CustomEmojiId::new(tone.emoji_id),
                title: tone.title,
                ..AiComposeTone::default()
            },
        };
        if result.author_user_id != crate::user_manager::UserId::default()
            && !result.author_user_id.is_valid()
        {
            tdutils::log_error!("Receive invalid author {:?}", result.author_user_id);
            result.author_user_id = crate::user_manager::UserId::default();
        }
        if result.custom_emoji_id != crate::emoji_status::CustomEmojiId::default()
            && !result.custom_emoji_id.is_valid()
        {
            tdutils::log_error!(
                "Receive invalid custom emoji id {:?}",
                result.custom_emoji_id
            );
            result.custom_emoji_id = crate::emoji_status::CustomEmojiId::default();
        }
        result
    }

    /// `AiComposeTone::get_text_composition_style_object`
    /// (AiComposeTone.cpp:58-63).
    pub fn get_text_composition_style_object(
        &self,
        env: &mut impl TranslationManagerEnv,
    ) -> td_api::TextCompositionStyle {
        td_api::TextCompositionStyle {
            name: self.slug.clone(),
            custom_emoji_id: self.custom_emoji_id.get(),
            title: self.title.clone(),
            is_custom: self.tone_type == AiComposeToneType::Custom,
            is_creator: self.is_creator,
            install_count: self.install_count,
            prompt: self.prompt.clone(),
            creator_user_id: env
                .get_user_id_object(self.author_user_id.get(), "textCompositionStyle"),
            english_example: self
                .english_example
                .get_text_composition_style_example_object(),
        }
    }

    /// `AiComposeTone::has_name` (AiComposeTone.h:47-49).
    pub fn has_name(&self, name: &str) -> bool {
        name == self.slug
    }

    /// `AiComposeTone::get_input_ai_compose_tone` (AiComposeTone.cpp:65-75).
    pub fn get_input_ai_compose_tone(&self) -> telegram_api::InputAiComposeTone {
        match self.tone_type {
            AiComposeToneType::Default => {
                telegram_api::InputAiComposeTone::inputAiComposeToneDefault(
                    telegram_api::inputAiComposeToneDefault {
                        tone: self.slug.clone(),
                    },
                )
            }
            AiComposeToneType::Custom => telegram_api::InputAiComposeTone::inputAiComposeToneID(
                telegram_api::inputAiComposeToneID {
                    id: self.id,
                    access_hash: self.access_hash,
                },
            ),
        }
    }

    /// `AiComposeTone::store` (AiComposeTone.hpp:17-60) — the flags word
    /// plus the conditionally-stored fields.
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_custom_emoji_id = self.custom_emoji_id.is_valid();
        let has_id = self.id != 0;
        let has_access_hash = self.access_hash != 0;
        let has_install_count = self.install_count != 0;
        let has_author_user_id = self.author_user_id != crate::user_manager::UserId::default();
        let has_prompt = !self.prompt.is_empty();
        let has_english_example = !self.english_example.is_empty();

        let mut flags = 0i32;
        if self.is_creator {
            flags |= 1;
        }
        if has_custom_emoji_id {
            flags |= 2;
        }
        if has_id {
            flags |= 4;
        }
        if has_access_hash {
            flags |= 8;
        }
        if has_install_count {
            flags |= 16;
        }
        if has_author_user_id {
            flags |= 32;
        }
        if has_prompt {
            flags |= 64;
        }
        if has_english_example {
            flags |= 128;
        }
        store_i32(out, flags);
        store_i32(out, self.tone_type as i32);
        store_string(out, &self.slug);
        if has_custom_emoji_id {
            store_i64(out, self.custom_emoji_id.get());
        }
        store_string(out, &self.title);
        if has_id {
            store_i64(out, self.id);
        }
        if has_access_hash {
            store_i64(out, self.access_hash);
        }
        if has_install_count {
            store_i32(out, self.install_count);
        }
        if has_author_user_id {
            store_i64(out, self.author_user_id.get());
        }
        if has_prompt {
            store_string(out, &self.prompt);
        }
        if has_english_example {
            self.english_example.store(out);
        }
    }

    /// `AiComposeTone::parse` (AiComposeTone.hpp:62-105).
    pub fn parse(input: &mut Parser<'_>) -> std::result::Result<Self, &'static str> {
        let flags = input.i32_()?;
        let mut result = AiComposeTone {
            tone_type: match input.i32_()? {
                0 => AiComposeToneType::Default,
                1 => AiComposeToneType::Custom,
                _ => return Err("Invalid AiComposeTone type"),
            },
            slug: input.string()?,
            ..AiComposeTone::default()
        };
        if flags & 1 != 0 {
            result.is_creator = true;
        }
        if flags & 2 != 0 {
            result.custom_emoji_id = crate::emoji_status::CustomEmojiId::new(input.i64_()?);
        }
        result.title = input.string()?;
        if flags & 4 != 0 {
            result.id = input.i64_()?;
        }
        if flags & 8 != 0 {
            result.access_hash = input.i64_()?;
        }
        if flags & 16 != 0 {
            result.install_count = input.i32_()?;
        }
        if flags & 32 != 0 {
            result.author_user_id = crate::user_manager::UserId::new(input.i64_()?);
        }
        if flags & 64 != 0 {
            result.prompt = input.string()?;
        }
        if flags & 128 != 0 {
            result.english_example = AiComposeToneExample::parse(input)?;
        }
        Ok(result)
    }
}

/// `td::AiComposeTones` (AiComposeTone.h:68-95).
#[derive(Clone, Debug, Default)]
pub struct AiComposeTones {
    tones: Vec<AiComposeTone>,
    hash: i64,
}

impl PartialEq for AiComposeTones {
    /// `operator==(const AiComposeTones &, const AiComposeTones &)`
    /// (AiComposeTone.cpp:126-128).
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash && self.tones == other.tones
    }
}

impl AiComposeTones {
    /// `AiComposeTones::AiComposeTones(Td *, telegram_api::... composes)`
    /// (AiComposeTone.cpp:89-96).
    pub fn new_from_telegram(
        env: &mut impl TranslationManagerEnv,
        tones: telegram_api::aicompose_tones,
    ) -> Self {
        env.on_get_users(tones.users, "AiComposeTones");
        AiComposeTones {
            tones: tones
                .tones
                .into_iter()
                .map(AiComposeTone::new_from_telegram)
                .collect(),
            hash: tones.hash,
        }
    }

    /// `AiComposeTones::get_update_text_composition_styles_object`
    /// (AiComposeTone.cpp:98-102).
    pub fn get_update_text_composition_styles_object(
        &self,
        env: &mut impl TranslationManagerEnv,
    ) -> td_api::Update {
        td_api::Update::updateTextCompositionStyles(td_api::updateTextCompositionStyles {
            styles: self
                .tones
                .iter()
                .map(|tone| tone.get_text_composition_style_object(env))
                .collect(),
        })
    }

    /// `AiComposeTones::get_hash` (AiComposeTone.h:81-83).
    pub fn get_hash(&self) -> i64 {
        self.hash
    }

    /// `AiComposeTones::get_input_ai_compose_tone` (AiComposeTone.cpp:
    /// 104-118) — the empty name is `nullptr`, the stored slugs win,
    /// a base64url name rides as the raw slug, everything else is the
    /// 400 "Style not found" band.
    pub fn get_input_ai_compose_tone(
        &self,
        name: &str,
    ) -> Result<Option<telegram_api::InputAiComposeTone>> {
        if name.is_empty() {
            return Result::Ok(None);
        }
        for tone in &self.tones {
            if tone.has_name(name) {
                return Result::Ok(Some(tone.get_input_ai_compose_tone()));
            }
        }
        if is_base64url_characters(name) {
            return Result::Ok(Some(
                telegram_api::InputAiComposeTone::inputAiComposeToneSlug(
                    telegram_api::inputAiComposeToneSlug {
                        slug: name.to_owned(),
                    },
                ),
            ));
        }
        Result::Err(Status::error(400, "Style not found"))
    }

    /// `AiComposeTones::add_dependencies` +
    /// `Dependencies::resolve_force` — the author ids the constructor
    /// restore must have loaded.
    pub fn author_user_ids(&self) -> Vec<i64> {
        self.tones
            .iter()
            .map(|tone| tone.author_user_id.get())
            .filter(|user_id| *user_id != 0)
            .collect()
    }

    /// `AiComposeTones::store` (AiComposeTone.hpp:107-113) — the empty
    /// flags word, the tones vector, the hash.
    pub fn store(&self, out: &mut Vec<u8>) {
        store_i32(out, 0);
        store_i32(out, self.tones.len() as i32);
        for tone in &self.tones {
            tone.store(out);
        }
        store_i64(out, self.hash);
    }

    /// `AiComposeTones::parse` (AiComposeTone.hpp:115-121).
    pub fn parse(input: &mut Parser<'_>) -> std::result::Result<Self, &'static str> {
        let flags = input.i32_()?;
        if flags != 0 {
            return Err("Invalid AiComposeTones flags");
        }
        let len = input.i32_()?;
        if len < 0 {
            return Err("Wrong vector length");
        }
        let mut tones = Vec::with_capacity(len as usize);
        for _ in 0..len {
            tones.push(AiComposeTone::parse(input)?);
        }
        let hash = input.i64_()?;
        input.fetch_end()?;
        Ok(AiComposeTones { tones, hash })
    }
}

/// `is_base64url_characters` (base64.cpp:150-202, the `is_url`
/// instantiation).
fn is_base64url_characters(input: &str) -> bool {
    input
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'=')
}

// ---------------------------------------------------------------------------
// The manager (TranslationManager.{h,cpp}).
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The manager (TranslationManager.{h,cpp}).
// ---------------------------------------------------------------------------

/// The `G()`/user slice the collapsed TranslationManager reads through —
/// the C++ manager reads the same surfaces off its `Td *td_` directly.
pub trait TranslationManagerEnv {
    // -- the G()/auth slice
    fn auth_is_authorized(&self) -> bool;
    fn auth_is_bot(&self) -> bool;
    fn close_flag(&self) -> bool;

    // -- the pmc slice
    /// `G()->td_db()->get_binlog_pmc()->get/set` — the raw form.
    fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;
    fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]);

    // -- the update plane
    /// `send_closure(G()->td(), &Td::send_update, ...)`.
    fn send_update(&mut self, update: td_api::Update);

    // -- the user plane (`UserManager *`)
    /// `user_manager_->on_get_users` — the aicompose.tones intake.
    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str);
    /// `user_manager_->get_user_id_object` — the td_api id out-converter.
    fn get_user_id_object(&mut self, user_id: i64, source: &str) -> i64;
    /// `Dependencies::resolve_force` over the tone author ids — whether
    /// every referenced user is loadable.
    fn resolve_user_dependencies(&mut self, user_ids: &[i64], source: &str) -> bool;
    /// `user_manager_->get_me` — the create/edit-tone pre-step.
    fn get_me(&mut self);

    // -- the net-plane query tails (the Td arms dispatch through the
    // -- `TranslationNetKind` plane, net-drain; the offline test env
    // -- drops the promise, which keeps the request unanswered)
    /// `TranslateTextQuery::send` (TranslationManager.cpp:34-85); the
    /// `message_full_id` flips the wire between the PEER form (a valid
    /// message id) and the TEXT form, and the
    /// `skip_bot_commands`/`max_media_timestamp` pair rides along for
    /// the `on_get_translated_texts` chain-in.
    #[allow(clippy::too_many_arguments)] // the C++ send signature has the same shape
    fn send_translate_text_query(
        &mut self,
        texts: Vec<FormattedText>,
        message_full_id: crate::messages_manager::MessageFullId,
        to_language_code: String,
        tone: String,
        skip_bot_commands: bool,
        max_media_timestamp: i32,
        promise: Promise<td_api::FormattedText>,
    );
    /// `ComposeMessageWithAiQuery::send` (TranslationManager.cpp:397-442)
    /// — the `InputText` carries the answer's
    /// `skip_bot_commands`/`max_media_timestamp` pair.
    fn send_compose_message_with_ai_query(
        &mut self,
        text: InputText,
        translate_to_language_code: String,
        input_tone: Option<telegram_api::InputAiComposeTone>,
        emojify: bool,
        promise: Promise<td_api::FormattedText>,
    );
    /// `ProofreadMessageWithAiQuery::send` (TranslationManager.cpp:529-568)
    /// — the `InputText` carries the answer's
    /// `skip_bot_commands`/`max_media_timestamp` pair.
    fn send_proofread_message_with_ai_query(
        &mut self,
        text: InputText,
        promise: Promise<td_api::FixedText>,
    );
    /// `GetAiComposeTonesQuery::send` (TranslationManager.cpp:175-202).
    fn send_get_ai_compose_tones_query(&mut self, hash: i64, promise: Promise<()>);
    /// `CreateToneQuery::send` (TranslationManager.cpp:204-234).
    fn send_create_tone_query(
        &mut self,
        title: String,
        custom_emoji_id: i64,
        prompt: String,
        show_creator: bool,
        promise: Promise<td_api::TextCompositionStyle>,
    );
    /// `UpdateToneQuery::send` (TranslationManager.cpp:236-270).
    fn send_update_tone_query(
        &mut self,
        input_tone: telegram_api::InputAiComposeTone,
        title: String,
        custom_emoji_id: i64,
        prompt: String,
        show_creator: bool,
        promise: Promise<td_api::TextCompositionStyle>,
    );
    /// `DeleteToneQuery::send` (TranslationManager.cpp:272-296).
    fn send_delete_tone_query(
        &mut self,
        input_tone: telegram_api::InputAiComposeTone,
        promise: Promise<()>,
    );
    /// `GetToneQuery::send` (TranslationManager.cpp:298-340).
    fn send_get_tone_query(
        &mut self,
        input_tone: telegram_api::InputAiComposeTone,
        promise: Promise<td_api::TextCompositionStyle>,
    );
    /// `GetToneExampleQuery::send` (TranslationManager.cpp:342-369).
    fn send_get_tone_example_query(
        &mut self,
        input_tone: telegram_api::InputAiComposeTone,
        num: i32,
        promise: Promise<td_api::TextCompositionStyleExample>,
    );
    /// `SaveToneQuery::send` (TranslationManager.cpp:371-395).
    fn send_save_tone_query(
        &mut self,
        input_tone: telegram_api::InputAiComposeTone,
        unsave: bool,
        promise: Promise<()>,
    );

    // -- the `[blocked: RichMessage]` rich-message intakes; the
    // -- `RichMessage::get_rich_message` input conversion is the
    // -- un-landed value plane, so the C++ request parks unanswered
    // -- offline exactly like a net tail
    /// `TranslateRichMessageQuery::send` (TranslationManager.cpp:87-173).
    fn send_translate_rich_message_query(
        &mut self,
        message: td_api::InputRichMessage,
        to_language_code: String,
        tone: String,
        promise: Promise<td_api::RichMessage>,
    );
    /// `do_compose_rich_message_with_ai` +
    /// `ComposeRichMessageWithAiQuery::send`
    /// (TranslationManager.cpp:444-527, 790-796) — the compose/create/
    /// fix rich-message family funnels here in the C++.
    #[allow(clippy::too_many_arguments)]
    fn send_compose_rich_message_with_ai_query(
        &mut self,
        has_message: bool,
        message: Option<td_api::InputRichMessage>,
        translate_to_language_code: String,
        input_tone: Option<telegram_api::InputAiComposeTone>,
        emojify: bool,
        proofread: bool,
        promise: Promise<td_api::RichMessage>,
    );
}

/// The parked TranslationManager promises (the net step): the C++ query
/// handlers own their promises; the port parks them at the Td-side lot
/// until the matching `TranslationNetKind` answer drains. The tone-CRUD
/// pair parks its payload for the answer conversion, and the reload
/// chain (`TonesReload`) resolves the parked `ToneStyle` entries when a
/// `aicompose.getTones` completion lands (the C++
/// `reload_ai_compose_tones` PromiseCreator lambda chain,
/// TranslationManager.cpp:225-227/261-263).
pub(crate) enum PendingTranslationQuery {
    /// The `TranslateTextQuery` promise (cpp:34-41).
    TranslateText {
        skip_bot_commands: bool,
        max_media_timestamp: i32,
        promise: Promise<td_api::FormattedText>,
    },
    /// The `ComposeMessageWithAiQuery` promise (cpp:397-406) — the
    /// skip/timestamp pair rides the kind payload.
    Compose {
        promise: Promise<td_api::FormattedText>,
    },
    /// The `ProofreadMessageWithAiQuery` promise (cpp:529-536) — the
    /// skip/timestamp pair rides the kind payload.
    Proofread { promise: Promise<td_api::FixedText> },
    /// The `GetAiComposeTonesQuery` promise — the reload surface
    /// (cpp:175-180); the delete/save tails park here through
    /// `reload_ai_compose_tones`.
    TonesReload { promise: Promise<()> },
    /// The `CreateToneQuery`/`UpdateToneQuery` promise pair (cpp:204-270)
    /// — the send parks `None`, the answer fills the converted style and
    /// the reload completion resolves it.
    ToneStyle {
        style: Option<td_api::TextCompositionStyle>,
        promise: Promise<td_api::TextCompositionStyle>,
    },
    /// The `DeleteToneQuery`/`SaveToneQuery` promise (cpp:272-295/371-395).
    ToneUnit { promise: Promise<()> },
    /// The `GetToneQuery` promise (cpp:298-305).
    GetTone {
        promise: Promise<td_api::TextCompositionStyle>,
    },
    /// The `GetToneExampleQuery` promise (cpp:342-348).
    ToneExample {
        promise: Promise<td_api::TextCompositionStyleExample>,
    },
}

impl PendingTranslationQuery {
    // The Td-side slot selectors (the star `take_pending` fn-ptr
    // convention) — one per parked family.
    pub(crate) fn is_translate_text(&self) -> bool {
        matches!(self, Self::TranslateText { .. })
    }
    pub(crate) fn is_compose(&self) -> bool {
        matches!(self, Self::Compose { .. })
    }
    pub(crate) fn is_proofread(&self) -> bool {
        matches!(self, Self::Proofread { .. })
    }
    pub(crate) fn is_tones_reload(&self) -> bool {
        matches!(self, Self::TonesReload { .. })
    }
    pub(crate) fn is_tone_style(&self) -> bool {
        matches!(self, Self::ToneStyle { .. })
    }
    pub(crate) fn is_tone_unit(&self) -> bool {
        matches!(self, Self::ToneUnit { .. })
    }
    pub(crate) fn is_get_tone(&self) -> bool {
        matches!(self, Self::GetTone { .. })
    }
    pub(crate) fn is_tone_example(&self) -> bool {
        matches!(self, Self::ToneExample { .. })
    }
}

/// `TranslationManager::InputText` (TranslationManager.h:38-42).
#[derive(Clone, Debug, Default)]
pub struct InputText {
    pub(crate) text: FormattedText,
    pub(crate) skip_bot_commands: bool,
    pub(crate) max_media_timestamp: i32,
}

/// The `TranslationManager` collapsed actor — the `Td`-owned struct with
/// the pmc-backed tone state
#[derive(Default)]
pub struct TranslationManager {
    /// `TranslationManager::ai_compose_tones_`.
    ai_compose_tones: AiComposeTones,
}

impl TranslationManager {
    /// `TranslationManager::start_up` (TranslationManager.cpp:573-592) —
    /// the constructor restore; runs in the `Td` init after the
    /// transcription manager's, in the C++ creation order.
    pub fn init(&mut self, env: &mut impl TranslationManagerEnv) {
        if !env.auth_is_authorized() || env.auth_is_bot() {
            return;
        }
        let blob = env.binlog_pmc_get_raw(AI_COMPOSE_TONES_KEY);
        if !blob.is_empty() {
            let mut parser = Parser::new(&blob);
            match AiComposeTones::parse(&mut parser) {
                Ok(tones) => {
                    let author_ids = tones.author_user_ids();
                    if env.resolve_user_dependencies(&author_ids, "AiComposeTones") {
                        self.ai_compose_tones = tones;
                    } else {
                        self.ai_compose_tones = AiComposeTones::default();
                    }
                }
                Err(_) => {
                    self.ai_compose_tones = AiComposeTones::default();
                }
            }
        }
        self.send_update_text_composition_styles(env);
        if self.ai_compose_tones == AiComposeTones::default() {
            self.reload_ai_compose_tones(env, Promise::default());
        }
    }

    /// `TranslationManager::on_authorization_success`
    /// (TranslationManager.cpp:598-603).
    pub fn on_authorization_success(&mut self, env: &mut impl TranslationManagerEnv) {
        if !env.auth_is_bot() {
            self.send_update_text_composition_styles(env);
            self.reload_ai_compose_tones(env, Promise::default());
        }
    }

    /// `TranslationManager::check_tone` (TranslationManager.cpp:605-612) —
    /// "neutral" clears to the default tone.
    pub fn check_tone(tone: &mut String) -> Result<()> {
        if tone == "neutral" {
            tone.clear();
        } else if !tone.is_empty() && tone != "formal" && tone != "casual" {
            return Result::Err(Status::error(400, "Invalid tone specified"));
        }
        Result::Ok(())
    }

    /// `TranslationManager::clone_input_ai_compose_tone`
    /// (TranslationManager.cpp:614-640).
    pub fn clone_input_ai_compose_tone(
        input_tone: Option<&telegram_api::InputAiComposeTone>,
    ) -> Option<telegram_api::InputAiComposeTone> {
        input_tone.cloned()
    }

    /// `TranslationManager::get_input_text` (TranslationManager.cpp:642-672)
    /// — the entity scan plus the M.8 fixups.
    pub fn get_input_text(
        &self,
        env: &mut (impl TranslationManagerEnv + crate::message_entity::UserManagerLike),
        text: Option<td_api::FormattedText>,
    ) -> Result<InputText> {
        let Some(mut text) = text else {
            return Result::Err(Status::error(400, "Text must be non-empty"));
        };
        let mut input_text = InputText {
            skip_bot_commands: true,
            max_media_timestamp: -1,
            ..InputText::default()
        };
        for entity in &text.entities {
            let Some(entity_type) = &entity.r#type else {
                continue;
            };
            match entity_type {
                td_api::TextEntityType::textEntityTypeBotCommand(_) => {
                    input_text.skip_bot_commands = false;
                }
                td_api::TextEntityType::textEntityTypeMediaTimestamp(entity_type) => {
                    input_text.max_media_timestamp = input_text
                        .max_media_timestamp
                        .max(entity_type.media_timestamp);
                }
                _ => {
                    // nothing to do
                }
            }
        }

        let mut entities = match get_message_entities(Some(env), text.entities, false) {
            Ok(entities) => entities,
            Err(status) => return Result::Err(status),
        };
        if let Err(status) = fix_formatted_text(
            &mut text.text,
            &mut entities,
            true, // allow_empty
            true, // allow_empty_string
            true, // skip_new_entities
            true, // skip_bot_commands
            true, // skip_media_timestamps
            true, // skip_trim
            None,
        ) {
            return Result::Err(status);
        }
        input_text.text = FormattedText {
            text: text.text,
            entities,
        };
        Result::Ok(input_text)
    }

    /// `TranslationManager::translate_text(td_api::formattedText, ...)`
    /// (TranslationManager.cpp:674-679) — the td_api-text form delegates
    /// to the message overload with the null full id (the TEXT-flag wire
    /// shape).
    pub fn translate_text(
        &self,
        env: &mut (impl TranslationManagerEnv + crate::message_entity::UserManagerLike),
        text: Option<td_api::FormattedText>,
        to_language_code: String,
        tone: String,
        mut promise: Promise<td_api::FormattedText>,
    ) {
        let input_text = match self.get_input_text(env, text) {
            Result::Ok(input_text) => input_text,
            Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        self.translate_input_text(
            env,
            input_text,
            crate::messages_manager::MessageFullId::default(),
            to_language_code,
            tone,
            promise,
        );
    }

    /// `TranslationManager::translate_text(InputText, MessageFullId, ...)`
    /// (TranslationManager.cpp:681-699) — the shared send body: the tone
    /// gate, then `TranslateTextQuery` with the message full id riding
    /// along (a valid id flips the wire to the PEER form; the §3.8
    /// net-drain 25 shape).
    pub fn translate_input_text(
        &self,
        env: &mut (impl TranslationManagerEnv + crate::message_entity::UserManagerLike),
        input_text: InputText,
        message_full_id: crate::messages_manager::MessageFullId,
        to_language_code: String,
        mut tone: String,
        mut promise: Promise<td_api::FormattedText>,
    ) {
        if let Result::Err(status) = Self::check_tone(&mut tone) {
            promise.set_error(status);
            return;
        }
        env.send_translate_text_query(
            vec![input_text.text],
            message_full_id,
            to_language_code,
            tone,
            input_text.skip_bot_commands,
            input_text.max_media_timestamp,
            promise,
        );
    }

    /// `TranslationManager::on_get_translated_texts`
    /// (TranslationManager.cpp:701-715) — the `TranslateTextQuery`
    /// answer side; the net step chains the parked promise in.
    pub fn on_get_translated_texts(
        &self,
        env: &mut (impl TranslationManagerEnv + crate::message_entity::UserManagerLike),
        texts: Vec<telegram_api::TextWithEntities>,
        skip_bot_commands: bool,
        max_media_timestamp: i32,
        mut promise: Promise<td_api::FormattedText>,
    ) {
        if env.close_flag() {
            promise.set_error(crate::config_result::request_aborted_error());
            return;
        }
        if texts.len() != 1 {
            if texts.is_empty() {
                promise.set_error(Status::error(500, "Translation failed"));
            } else {
                promise.set_error(Status::error(500, "Receive invalid number of results"));
            }
            return;
        }
        let text = texts.into_iter().next().unwrap();
        let formatted_text = get_formatted_text(
            Some(env),
            text.text,
            text.entities,
            max_media_timestamp == -1,
            true,
            "on_get_translated_texts",
        );
        promise.set_value(get_formatted_text_object(
            Some(env),
            &formatted_text,
            skip_bot_commands,
            max_media_timestamp,
        ));
    }

    /// `TranslationManager::translate_rich_message`
    /// (TranslationManager.cpp:727-751) — [blocked: RichMessage]; the
    /// tone check runs first (the only reachable offline band; the C++
    /// validates the rich input before the tone), then the intake parks
    /// at the seam.
    pub fn translate_rich_message(
        &self,
        env: &mut impl TranslationManagerEnv,
        message: Option<td_api::InputRichMessage>,
        to_language_code: String,
        mut tone: String,
        mut promise: Promise<td_api::RichMessage>,
    ) {
        if let Result::Err(status) = Self::check_tone(&mut tone) {
            promise.set_error(status);
            return;
        }
        let Some(message) = message else {
            // the C++ get_input_rich_message null band; parked at the seam
            tdutils::log_info!("Empty rich message — [blocked: RichMessage]");
            return;
        };
        env.send_translate_rich_message_query(message, to_language_code, tone, promise);
    }

    /// `TranslationManager::compose_message_with_ai`
    /// (TranslationManager.cpp:775-782).
    pub fn compose_message_with_ai(
        &self,
        env: &mut (impl TranslationManagerEnv + crate::message_entity::UserManagerLike),
        text: Option<td_api::FormattedText>,
        translate_to_language_code: String,
        tone: String,
        emojify: bool,
        mut promise: Promise<td_api::FormattedText>,
    ) {
        let input_text = match self.get_input_text(env, text) {
            Result::Ok(input_text) => input_text,
            Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let input_tone = match self.ai_compose_tones.get_input_ai_compose_tone(&tone) {
            Result::Ok(input_tone) => input_tone,
            Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        env.send_compose_message_with_ai_query(
            input_text,
            translate_to_language_code,
            input_tone,
            emojify,
            promise,
        );
    }

    /// `TranslationManager::proofread_message_with_ai`
    /// (TranslationManager.cpp:784-788).
    pub fn proofread_message_with_ai(
        &self,
        env: &mut (impl TranslationManagerEnv + crate::message_entity::UserManagerLike),
        text: Option<td_api::FormattedText>,
        mut promise: Promise<td_api::FixedText>,
    ) {
        let input_text = match self.get_input_text(env, text) {
            Result::Ok(input_text) => input_text,
            Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        env.send_proofread_message_with_ai_query(input_text, promise);
    }

    /// `TranslationManager::compose_rich_message_with_ai`
    /// (TranslationManager.cpp:798-811) — [blocked: RichMessage].
    #[allow(clippy::too_many_arguments)]
    pub fn compose_rich_message_with_ai(
        &self,
        env: &mut impl TranslationManagerEnv,
        message: Option<td_api::InputRichMessage>,
        translate_to_language_code: String,
        tone: String,
        custom_prompt: String,
        emojify: bool,
        mut promise: Promise<td_api::RichMessage>,
    ) {
        let input_tone = if custom_prompt.is_empty() {
            match self.ai_compose_tones.get_input_ai_compose_tone(&tone) {
                Result::Ok(input_tone) => input_tone,
                Result::Err(status) => {
                    promise.set_error(status);
                    return;
                }
            }
        } else {
            Some(
                telegram_api::InputAiComposeTone::inputAiComposeToneSingleUse(
                    telegram_api::inputAiComposeToneSingleUse { custom_prompt },
                ),
            )
        };
        env.send_compose_rich_message_with_ai_query(
            true,
            message,
            translate_to_language_code,
            input_tone,
            emojify,
            false,
            promise,
        );
    }

    /// `TranslationManager::create_rich_message_with_ai`
    /// (TranslationManager.cpp:813-818) — [blocked: RichMessage].
    pub fn create_rich_message_with_ai(
        &self,
        env: &mut impl TranslationManagerEnv,
        prompt: String,
        language_code: String,
        emojify: bool,
        promise: Promise<td_api::RichMessage>,
    ) {
        let input_tone = telegram_api::InputAiComposeTone::inputAiComposeToneSingleUse(
            telegram_api::inputAiComposeToneSingleUse {
                custom_prompt: prompt,
            },
        );
        env.send_compose_rich_message_with_ai_query(
            false,
            None,
            language_code,
            Some(input_tone),
            emojify,
            false,
            promise,
        );
    }

    /// `TranslationManager::proofread_rich_message_with_ai`
    /// (TranslationManager.cpp:820-825) — [blocked: RichMessage].
    pub fn proofread_rich_message_with_ai(
        &self,
        env: &mut impl TranslationManagerEnv,
        message: Option<td_api::InputRichMessage>,
        promise: Promise<td_api::RichMessage>,
    ) {
        env.send_compose_rich_message_with_ai_query(
            true,
            message,
            String::new(),
            None,
            false,
            true,
            promise,
        );
    }

    /// `TranslationManager::get_recognized_speech_input_ai_compose_tone`
    /// (TranslationManager.cpp:827-830) — the shared tone of the
    /// fix-recognized pair: the default tone with the "voice" prompt.
    fn get_recognized_speech_input_ai_compose_tone() -> Option<telegram_api::InputAiComposeTone> {
        Some(telegram_api::InputAiComposeTone::inputAiComposeToneDefault(
            telegram_api::inputAiComposeToneDefault {
                tone: "voice".to_string(),
            },
        ))
    }

    /// `TranslationManager::fix_recognized_text_with_ai`
    /// (TranslationManager.cpp:832-838) — the recognized-text fix rides
    /// the same `messages.composeMessageWithAI` query as the compose
    /// family, with no language code, the "voice" tone and no emojify.
    pub fn fix_recognized_text_with_ai(
        &self,
        env: &mut (impl TranslationManagerEnv + crate::message_entity::UserManagerLike),
        text: Option<td_api::FormattedText>,
        mut promise: Promise<td_api::FormattedText>,
    ) {
        let input_text = match self.get_input_text(env, text) {
            Result::Ok(input_text) => input_text,
            Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        env.send_compose_message_with_ai_query(
            input_text,
            String::new(),
            Self::get_recognized_speech_input_ai_compose_tone(),
            false,
            promise,
        );
    }

    /// `TranslationManager::fix_recognized_rich_message_with_ai`
    /// (TranslationManager.cpp:840-848) — [blocked: RichMessage], like
    /// the rest of the rich family.
    pub fn fix_recognized_rich_message_with_ai(
        &self,
        env: &mut impl TranslationManagerEnv,
        message: Option<td_api::InputRichMessage>,
        promise: Promise<td_api::RichMessage>,
    ) {
        env.send_compose_rich_message_with_ai_query(
            true,
            message,
            String::new(),
            Self::get_recognized_speech_input_ai_compose_tone(),
            false,
            false,
            promise,
        );
    }

    /// `TranslationManager::reload_ai_compose_tones`
    /// (TranslationManager.cpp:831-835).
    pub fn reload_ai_compose_tones(
        &self,
        env: &mut impl TranslationManagerEnv,
        promise: Promise<()>,
    ) {
        if env.auth_is_authorized() && !env.auth_is_bot() {
            env.send_get_ai_compose_tones_query(self.ai_compose_tones.get_hash(), promise);
        }
    }

    /// `TranslationManager::on_get_ai_compose_tones`
    /// (TranslationManager.cpp:837-860) — the
    /// `GetAiComposeTonesQuery` answer side.
    pub fn on_get_ai_compose_tones(
        &mut self,
        env: &mut impl TranslationManagerEnv,
        tones: telegram_api::aicompose_Tones,
    ) {
        if !env.auth_is_authorized() {
            return;
        }
        match tones {
            telegram_api::aicompose_Tones::aicompose_tonesNotModified(_) => {
                // nothing to do
            }
            telegram_api::aicompose_Tones::aicompose_tones(tones) => {
                let ai_compose_tones = AiComposeTones::new_from_telegram(env, tones);
                if ai_compose_tones == self.ai_compose_tones {
                    return;
                }
                self.ai_compose_tones = ai_compose_tones;
                let mut blob = Vec::new();
                self.ai_compose_tones.store(&mut blob);
                env.binlog_pmc_set_raw(AI_COMPOSE_TONES_KEY, &blob);
                self.send_update_text_composition_styles(env);
            }
        }
    }

    /// `TranslationManager::create_tone` +
    /// `do_create_tone` (TranslationManager.cpp:862-877) — the `get_me`
    /// pre-step then the query.
    pub fn create_tone(
        &self,
        env: &mut impl TranslationManagerEnv,
        title: String,
        custom_emoji_id: i64,
        prompt: String,
        show_creator: bool,
        mut promise: Promise<td_api::TextCompositionStyle>,
    ) {
        env.get_me();
        if env.close_flag() {
            promise.set_error(crate::config_result::request_aborted_error());
            return;
        }
        env.send_create_tone_query(title, custom_emoji_id, prompt, show_creator, promise);
    }

    /// `TranslationManager::update_tone` + `do_update_tone`
    /// (TranslationManager.cpp:879-896).
    #[allow(clippy::too_many_arguments)]
    pub fn update_tone(
        &self,
        env: &mut impl TranslationManagerEnv,
        name: String,
        title: String,
        custom_emoji_id: i64,
        prompt: String,
        show_creator: bool,
        mut promise: Promise<td_api::TextCompositionStyle>,
    ) {
        env.get_me();
        if env.close_flag() {
            promise.set_error(crate::config_result::request_aborted_error());
            return;
        }
        let input_tone = match self.ai_compose_tones.get_input_ai_compose_tone(&name) {
            Result::Ok(input_tone) => input_tone,
            Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let Some(input_tone) = input_tone else {
            // the C++ create_handler would CHECK; the empty name only
            // reaches the net tail with an empty lookup, which the tone
            // family never does offline
            tdutils::log_info!("updateTone with an empty style name");
            return;
        };
        env.send_update_tone_query(
            input_tone,
            title,
            custom_emoji_id,
            prompt,
            show_creator,
            promise,
        );
    }

    /// `TranslationManager::delete_tone` (TranslationManager.cpp:898-901).
    pub fn delete_tone(
        &self,
        env: &mut impl TranslationManagerEnv,
        name: String,
        mut promise: Promise<()>,
    ) {
        let input_tone = match self.resolve_family_tone(env, &name, &mut promise) {
            Some(input_tone) => input_tone,
            None => return,
        };
        env.send_delete_tone_query(input_tone, promise);
    }

    /// `TranslationManager::search_tone` (TranslationManager.cpp:903-907).
    pub fn search_tone(
        &self,
        env: &mut impl TranslationManagerEnv,
        name: String,
        mut promise: Promise<td_api::TextCompositionStyle>,
    ) {
        let input_tone = match self.resolve_family_tone(env, &name, &mut promise) {
            Some(input_tone) => input_tone,
            None => return,
        };
        env.send_get_tone_query(input_tone, promise);
    }

    /// `TranslationManager::get_tone_example`
    /// (TranslationManager.cpp:909-913).
    pub fn get_tone_example(
        &self,
        env: &mut impl TranslationManagerEnv,
        name: String,
        num: i32,
        mut promise: Promise<td_api::TextCompositionStyleExample>,
    ) {
        let input_tone = match self.resolve_family_tone(env, &name, &mut promise) {
            Some(input_tone) => input_tone,
            None => return,
        };
        env.send_get_tone_example_query(input_tone, num, promise);
    }

    /// `TranslationManager::add_tone` (TranslationManager.cpp:915-918).
    pub fn add_tone(
        &self,
        env: &mut impl TranslationManagerEnv,
        name: String,
        mut promise: Promise<()>,
    ) {
        let input_tone = match self.resolve_family_tone(env, &name, &mut promise) {
            Some(input_tone) => input_tone,
            None => return,
        };
        env.send_save_tone_query(input_tone, false, promise);
    }

    /// `TranslationManager::remove_tone` (TranslationManager.cpp:920-923).
    pub fn remove_tone(
        &self,
        env: &mut impl TranslationManagerEnv,
        name: String,
        mut promise: Promise<()>,
    ) {
        let input_tone = match self.resolve_family_tone(env, &name, &mut promise) {
            Some(input_tone) => input_tone,
            None => return,
        };
        env.send_save_tone_query(input_tone, true, promise);
    }

    /// The tone-family lookup with the promise-band error: the shared
    /// `TRY_RESULT_PROMISE(promise, input_tone,
    /// ai_compose_tones_.get_input_ai_compose_tone(name))` shape. `None`
    /// (the empty name, the C++ `nullptr`) drops the promise like the
    /// C++ `create_handler` CHECK unreachable.
    fn resolve_family_tone<T: Send + 'static>(
        &self,
        _env: &mut impl TranslationManagerEnv,
        name: &str,
        promise: &mut Promise<T>,
    ) -> Option<telegram_api::InputAiComposeTone> {
        match self.ai_compose_tones.get_input_ai_compose_tone(name) {
            Result::Ok(Some(input_tone)) => Some(input_tone),
            // The empty name (the C++ `nullptr`) rides to the seam; the
            // seam's drop keeps the C++ create_handler CHECK unreachable
            // band parked offline.
            Result::Ok(None) => None,
            Result::Err(status) => {
                promise.set_error(status);
                None
            }
        }
    }

    /// `TranslationManager::send_update_text_composition_styles`
    /// (TranslationManager.cpp:925-927).
    pub fn send_update_text_composition_styles(&self, env: &mut impl TranslationManagerEnv) {
        let update = self
            .ai_compose_tones
            .get_update_text_composition_styles_object(env);
        env.send_update(update);
    }

    /// `TranslationManager::get_current_state`
    /// (TranslationManager.cpp:929-935).
    pub fn get_current_state(
        &self,
        env: &mut impl TranslationManagerEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        if !env.auth_is_authorized() || env.auth_is_bot() {
            return;
        }
        let update = self
            .ai_compose_tones
            .get_update_text_composition_styles_object(env);
        updates.push(update);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    use tdutils::promise::{Promise, PromiseInterface};

    /// A [`PromiseInterface`] that flips flags instead of answering
    /// requests — the offline stand-in for the deferred outbox.
    struct FlagPromise<T: Send + 'static> {
        value_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
        error_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
        _type: std::marker::PhantomData<T>,
    }

    impl<T: Send + 'static> FlagPromise<T> {
        fn create() -> (
            (
                std::sync::Arc<std::sync::atomic::AtomicBool>,
                std::sync::Arc<std::sync::atomic::AtomicBool>,
            ),
            Promise<T>,
        ) {
            let value_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let error_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            (
                (value_flag.clone(), error_flag.clone()),
                Promise::from_interface(FlagPromise {
                    value_flag,
                    error_flag,
                    _type: std::marker::PhantomData,
                }),
            )
        }
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

    fn flag(f: &std::sync::atomic::AtomicBool) -> bool {
        f.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The offline env: the pmc state in a plain map, the recorded
    /// net-plane query tails (the Td arms dispatch; the tests park) and
    /// the `[blocked: RichMessage]` rich-message tails, and the user
    /// plane answered by a synchronous empty cache.
    struct TestTranslationEnv {
        pmc: HashMap<Vec<u8>, Vec<u8>>,
        authorized: bool,
        bot: bool,
        close_flag: bool,
        users_seen: usize,
        me_calls: i32,
        queries: Vec<&'static str>,
        last_skip_bot_commands: bool,
        last_max_media_timestamp: i32,
        /// the recorded `send_compose_message_with_ai_query` arguments
        last_compose_lang: String,
        last_compose_tone: Option<String>,
        last_compose_emojify: bool,
        updates: Vec<td_api::Update>,
    }

    impl TestTranslationEnv {
        fn new() -> TestTranslationEnv {
            TestTranslationEnv {
                pmc: HashMap::new(),
                authorized: true,
                bot: false,
                close_flag: false,
                users_seen: 0,
                me_calls: 0,
                queries: Vec::new(),
                last_skip_bot_commands: true,
                last_max_media_timestamp: -1,
                last_compose_lang: String::new(),
                last_compose_tone: None,
                last_compose_emojify: false,
                updates: Vec::new(),
            }
        }
    }

    impl TranslationManagerEnv for TestTranslationEnv {
        fn auth_is_authorized(&self) -> bool {
            self.authorized
        }

        fn auth_is_bot(&self) -> bool {
            self.bot
        }

        fn close_flag(&self) -> bool {
            self.close_flag
        }

        fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
            self.pmc.get(key).cloned().unwrap_or_default()
        }

        fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
            self.pmc.insert(key.to_vec(), value.to_vec());
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn on_get_users(&mut self, users: Vec<telegram_api::User>, _source: &str) {
            self.users_seen += users.len();
        }

        fn get_user_id_object(&mut self, user_id: i64, _source: &str) -> i64 {
            user_id
        }

        fn resolve_user_dependencies(&mut self, _user_ids: &[i64], _source: &str) -> bool {
            true
        }

        fn get_me(&mut self) {
            self.me_calls += 1;
        }

        fn send_translate_text_query(
            &mut self,
            _texts: Vec<FormattedText>,
            _message_full_id: crate::messages_manager::MessageFullId,
            _to_language_code: String,
            _tone: String,
            skip_bot_commands: bool,
            max_media_timestamp: i32,
            _promise: Promise<td_api::FormattedText>,
        ) {
            self.queries.push("translate");
            self.last_skip_bot_commands = skip_bot_commands;
            self.last_max_media_timestamp = max_media_timestamp;
        }

        fn send_compose_message_with_ai_query(
            &mut self,
            _text: InputText,
            translate_to_language_code: String,
            input_tone: Option<telegram_api::InputAiComposeTone>,
            emojify: bool,
            _promise: Promise<td_api::FormattedText>,
        ) {
            self.queries.push("compose");
            self.last_compose_lang = translate_to_language_code;
            self.last_compose_tone = match input_tone {
                Some(telegram_api::InputAiComposeTone::inputAiComposeToneDefault(tone)) => {
                    Some(tone.tone)
                }
                Some(telegram_api::InputAiComposeTone::inputAiComposeToneSingleUse(tone)) => {
                    Some(tone.custom_prompt)
                }
                Some(_) => Some(String::new()),
                None => None,
            };
            self.last_compose_emojify = emojify;
        }

        fn send_proofread_message_with_ai_query(
            &mut self,
            _text: InputText,
            _promise: Promise<td_api::FixedText>,
        ) {
            self.queries.push("proofread");
        }

        fn send_get_ai_compose_tones_query(&mut self, _hash: i64, _promise: Promise<()>) {
            self.queries.push("get_tones");
        }

        fn send_create_tone_query(
            &mut self,
            _title: String,
            _custom_emoji_id: i64,
            _prompt: String,
            _show_creator: bool,
            _promise: Promise<td_api::TextCompositionStyle>,
        ) {
            self.queries.push("create_tone");
        }

        fn send_update_tone_query(
            &mut self,
            _input_tone: telegram_api::InputAiComposeTone,
            _title: String,
            _custom_emoji_id: i64,
            _prompt: String,
            _show_creator: bool,
            _promise: Promise<td_api::TextCompositionStyle>,
        ) {
            self.queries.push("update_tone");
        }

        fn send_delete_tone_query(
            &mut self,
            _input_tone: telegram_api::InputAiComposeTone,
            _promise: Promise<()>,
        ) {
            self.queries.push("delete_tone");
        }

        fn send_get_tone_query(
            &mut self,
            _input_tone: telegram_api::InputAiComposeTone,
            _promise: Promise<td_api::TextCompositionStyle>,
        ) {
            self.queries.push("get_tone");
        }

        fn send_get_tone_example_query(
            &mut self,
            _input_tone: telegram_api::InputAiComposeTone,
            _num: i32,
            _promise: Promise<td_api::TextCompositionStyleExample>,
        ) {
            self.queries.push("get_tone_example");
        }

        fn send_save_tone_query(
            &mut self,
            _input_tone: telegram_api::InputAiComposeTone,
            _unsave: bool,
            _promise: Promise<()>,
        ) {
            self.queries.push("save_tone");
        }

        fn send_translate_rich_message_query(
            &mut self,
            _message: td_api::InputRichMessage,
            _to_language_code: String,
            _tone: String,
            _promise: Promise<td_api::RichMessage>,
        ) {
            self.queries.push("translate_rich");
        }

        fn send_compose_rich_message_with_ai_query(
            &mut self,
            _has_message: bool,
            _message: Option<td_api::InputRichMessage>,
            _translate_to_language_code: String,
            _input_tone: Option<telegram_api::InputAiComposeTone>,
            _emojify: bool,
            _proofread: bool,
            _promise: Promise<td_api::RichMessage>,
        ) {
            self.queries.push("compose_rich");
        }
    }

    impl crate::message_entity::UserManagerLike for TestTranslationEnv {
        fn resolve_user_id_object(
            &mut self,
            _user_id: crate::user_manager::UserId,
            _source: &str,
        ) -> i64 {
            0
        }

        fn ensure_input_user(
            &mut self,
            _user_id: crate::user_manager::UserId,
        ) -> std::result::Result<(), Status> {
            Ok(())
        }

        fn have_min_user(&self, _user_id: crate::user_manager::UserId) -> bool {
            true
        }

        fn get_input_user_force(
            &mut self,
            _user_id: crate::user_manager::UserId,
        ) -> telegram_api::InputUser {
            telegram_api::InputUser::inputUserEmpty(telegram_api::inputUserEmpty {})
        }

        fn is_user_bot(&self, _user_id: crate::user_manager::UserId) -> bool {
            false
        }
    }

    /// The custom tone the restore/intake tests share.
    fn custom_tone() -> AiComposeTone {
        AiComposeTone {
            tone_type: AiComposeToneType::Custom,
            slug: "slg".to_owned(),
            custom_emoji_id: crate::emoji_status::CustomEmojiId::new(77),
            title: "T".to_owned(),
            is_creator: true,
            id: 1001,
            access_hash: 2002,
            install_count: 3,
            prompt: "p".to_owned(),
            author_user_id: crate::user_manager::UserId::new(42),
            english_example: AiComposeToneExample::default(),
        }
    }

    /// The `td_api::formattedText` request input, entities empty.
    fn api_text(text: &str) -> Option<td_api::FormattedText> {
        Some(td_api::FormattedText {
            text: text.to_owned(),
            entities: Vec::new(),
        })
    }

    /// The styles vector of an `updateTextCompositionStyles` update.
    fn styles_of(update: &td_api::Update) -> &[td_api::TextCompositionStyle] {
        match update {
            td_api::Update::updateTextCompositionStyles(update) => &update.styles,
            _ => panic!("unexpected update {update:?}"),
        }
    }

    #[test]
    fn check_tone_bands_follow_the_cpp_table() {
        let mut tone = "neutral".to_owned();
        assert!(matches!(
            TranslationManager::check_tone(&mut tone),
            Result::Ok(())
        ));
        assert_eq!(tone, "");
        for ok in ["", "formal", "casual"] {
            let mut tone = ok.to_owned();
            assert!(matches!(
                TranslationManager::check_tone(&mut tone),
                Result::Ok(())
            ));
        }
        let mut bogus = "bogus".to_owned();
        assert!(matches!(
            TranslationManager::check_tone(&mut bogus),
            Result::Err(_)
        ));
        assert_eq!(bogus, "bogus");
    }

    #[test]
    fn ai_compose_tone_example_store_parse_round_trips() {
        let example = AiComposeToneExample {
            from_text: FormattedText {
                text: "from".to_owned(),
                entities: Vec::new(),
            },
            to_text: FormattedText {
                text: "to".to_owned(),
                entities: Vec::new(),
            },
        };
        let mut out = Vec::new();
        example.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = AiComposeToneExample::parse(&mut parser).unwrap();
        assert_eq!(parsed, example);
    }

    #[test]
    fn ai_compose_tone_store_parse_round_trips() {
        let tone = custom_tone();
        let mut out = Vec::new();
        tone.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = AiComposeTone::parse(&mut parser).unwrap();
        assert_eq!(parsed, tone);
    }

    #[test]
    fn ai_compose_tones_store_parse_round_trips_with_the_hash() {
        let tones = AiComposeTones {
            tones: vec![custom_tone()],
            hash: 5,
        };
        let mut out = Vec::new();
        tones.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = AiComposeTones::parse(&mut parser).unwrap();
        assert_eq!(parsed, tones);
        assert_eq!(parsed.get_hash(), 5);
        assert_eq!(parsed.author_user_ids(), vec![42]);
    }

    #[test]
    fn init_unauthorized_is_inert() {
        let mut env = TestTranslationEnv::new();
        env.authorized = false;
        let mut manager = TranslationManager::default();
        manager.init(&mut env);
        assert!(env.updates.is_empty());
        assert!(env.queries.is_empty());
    }

    #[test]
    fn init_restores_custom_tones_from_pmc() {
        let mut env = TestTranslationEnv::new();
        let tones = AiComposeTones {
            tones: vec![custom_tone()],
            hash: 5,
        };
        let mut blob = Vec::new();
        tones.store(&mut blob);
        env.pmc.insert(AI_COMPOSE_TONES_KEY.to_vec(), blob);

        let mut manager = TranslationManager::default();
        manager.init(&mut env);

        // The restored styles go out; the cache is non-empty, so no
        // reload query rides along.
        assert_eq!(env.updates.len(), 1);
        let styles = styles_of(&env.updates[0]);
        assert_eq!(styles.len(), 1);
        assert_eq!(styles[0].name, "slg");
        assert!(styles[0].is_custom);
        assert_eq!(styles[0].creator_user_id, 42);
        assert!(env.queries.is_empty());

        // The restored slug resolves through the tone lookup.
        let ((value_flag, error_flag), promise) = FlagPromise::<td_api::FormattedText>::create();
        manager.compose_message_with_ai(
            &mut env,
            api_text("hi"),
            "es".to_owned(),
            "slg".to_owned(),
            false,
            promise,
        );
        assert_eq!(env.queries, vec!["compose"]);
        assert!(!flag(&value_flag));
        assert!(!flag(&error_flag));
    }

    #[test]
    fn init_corrupt_blob_resets_to_default_and_reloads() {
        let mut env = TestTranslationEnv::new();
        env.pmc
            .insert(AI_COMPOSE_TONES_KEY.to_vec(), b"junk".to_vec());

        let mut manager = TranslationManager::default();
        manager.init(&mut env);

        assert_eq!(env.updates.len(), 1);
        assert!(styles_of(&env.updates[0]).is_empty());
        assert_eq!(env.queries, vec!["get_tones"]);
    }

    #[test]
    fn translate_text_parks_the_promise_offline() {
        let mut env = TestTranslationEnv::new();
        let manager = TranslationManager::default();
        let ((value_flag, error_flag), promise) = FlagPromise::<td_api::FormattedText>::create();
        manager.translate_text(
            &mut env,
            api_text("hi"),
            "es".to_owned(),
            String::new(),
            promise,
        );
        assert_eq!(env.queries, vec!["translate"]);
        assert!(env.last_skip_bot_commands);
        assert_eq!(env.last_max_media_timestamp, -1);
        assert!(!flag(&value_flag));
        assert!(!flag(&error_flag));
    }

    #[test]
    fn translate_text_scans_the_entity_plane() {
        let mut env = TestTranslationEnv::new();
        let manager = TranslationManager::default();
        let text = Some(td_api::FormattedText {
            text: "hi".to_owned(),
            entities: vec![td_api::TextEntity {
                offset: 0,
                length: 2,
                r#type: Some(td_api::TextEntityType::textEntityTypeMediaTimestamp(
                    td_api::textEntityTypeMediaTimestamp {
                        media_timestamp: 42,
                    },
                )),
            }],
        });
        let ((_value_flag, _error_flag), promise) = FlagPromise::<td_api::FormattedText>::create();
        manager.translate_text(&mut env, text, "es".to_owned(), String::new(), promise);
        assert_eq!(env.queries, vec!["translate"]);
        assert_eq!(env.last_max_media_timestamp, 42);
    }

    #[test]
    fn fix_recognized_text_with_ai_rides_the_compose_query() {
        let mut env = TestTranslationEnv::new();
        let manager = TranslationManager::default();
        let ((_value_flag, _error_flag), promise) = FlagPromise::<td_api::FormattedText>::create();
        manager.fix_recognized_text_with_ai(&mut env, api_text("recog nized"), promise);
        assert_eq!(env.queries, vec!["compose"]);
        // no language code, the "voice" default tone, no emojify — the
        // recognized-speech form of the shared compose query
        // (TranslationManager.cpp:832-838).
        assert_eq!(env.last_compose_lang, "");
        assert_eq!(env.last_compose_tone.as_deref(), Some("voice"));
        assert!(!env.last_compose_emojify);

        // the empty-text band answers before the query (the
        // get_input_text null band).
        let mut env = TestTranslationEnv::new();
        let manager = TranslationManager::default();
        let ((_value_flag, error_flag), promise) = FlagPromise::<td_api::FormattedText>::create();
        manager.fix_recognized_text_with_ai(&mut env, None, promise);
        assert!(env.queries.is_empty());
        assert!(flag(&error_flag));
    }

    #[test]
    fn fix_recognized_rich_message_with_ai_parks_at_the_seam() {
        // [blocked: RichMessage] — the promise drops unanswered, like
        // the rest of the rich family.
        let mut env = TestTranslationEnv::new();
        let manager = TranslationManager::default();
        let ((_value_flag, _error_flag), promise) = FlagPromise::<td_api::RichMessage>::create();
        manager.fix_recognized_rich_message_with_ai(&mut env, None, promise);
        assert_eq!(env.queries, vec!["compose_rich"]);
    }

    #[test]
    fn translate_text_rejects_a_bogus_tone_before_the_seam() {
        let mut env = TestTranslationEnv::new();
        let manager = TranslationManager::default();
        let ((_value_flag, error_flag), promise) = FlagPromise::<td_api::FormattedText>::create();
        manager.translate_text(
            &mut env,
            api_text("hi"),
            "es".to_owned(),
            "bogus".to_owned(),
            promise,
        );
        assert!(env.queries.is_empty());
        assert!(flag(&error_flag));
    }

    #[test]
    fn translate_text_requires_text() {
        let mut env = TestTranslationEnv::new();
        let manager = TranslationManager::default();
        let ((_value_flag, error_flag), promise) = FlagPromise::<td_api::FormattedText>::create();
        manager.translate_text(&mut env, None, "es".to_owned(), String::new(), promise);
        assert!(env.queries.is_empty());
        assert!(flag(&error_flag));
    }

    #[test]
    fn compose_message_with_ai_rejects_an_unknown_style_name() {
        let mut env = TestTranslationEnv::new();
        let manager = TranslationManager::default();
        let ((_value_flag, error_flag), promise) = FlagPromise::<td_api::FormattedText>::create();
        // "unknown!" is not base64url, so it rides past the slug band
        // into the 400 "Style not found" error.
        manager.compose_message_with_ai(
            &mut env,
            api_text("hi"),
            "es".to_owned(),
            "unknown!".to_owned(),
            false,
            promise,
        );
        assert!(env.queries.is_empty());
        assert!(flag(&error_flag));
    }

    #[test]
    fn compose_message_with_ai_rides_the_empty_default_tone() {
        let mut env = TestTranslationEnv::new();
        let manager = TranslationManager::default();
        let ((_value_flag, _error_flag), promise) = FlagPromise::<td_api::FormattedText>::create();
        manager.compose_message_with_ai(
            &mut env,
            api_text("hi"),
            "es".to_owned(),
            String::new(),
            true,
            promise,
        );
        assert_eq!(env.queries, vec!["compose"]);
    }

    #[test]
    fn proofread_message_with_ai_hits_the_seam() {
        let mut env = TestTranslationEnv::new();
        let manager = TranslationManager::default();
        let ((_value_flag, _error_flag), promise) = FlagPromise::<td_api::FixedText>::create();
        manager.proofread_message_with_ai(&mut env, api_text("hi"), promise);
        assert_eq!(env.queries, vec!["proofread"]);
    }

    #[test]
    fn on_get_translated_texts_sets_the_value() {
        let mut env = TestTranslationEnv::new();
        let manager = TranslationManager::default();
        let ((value_flag, _error_flag), promise) = FlagPromise::<td_api::FormattedText>::create();
        let texts = vec![telegram_api::TextWithEntities {
            text: "hola".to_owned(),
            entities: Vec::new(),
        }];
        manager.on_get_translated_texts(&mut env, texts, true, -1, promise);
        assert!(flag(&value_flag));
    }

    #[test]
    fn on_get_translated_texts_rejects_a_wrong_count() {
        let mut env = TestTranslationEnv::new();
        let manager = TranslationManager::default();
        let ((_value_flag, error_flag), promise) = FlagPromise::<td_api::FormattedText>::create();
        manager.on_get_translated_texts(&mut env, Vec::new(), true, -1, promise);
        assert!(flag(&error_flag));
    }

    #[test]
    fn rich_intake_parks_without_a_message() {
        let mut env = TestTranslationEnv::new();
        let manager = TranslationManager::default();
        let ((value_flag, error_flag), promise) = FlagPromise::<td_api::RichMessage>::create();
        manager.translate_rich_message(&mut env, None, "es".to_owned(), String::new(), promise);
        // The C++ parks the promise unanswered (the null rich-message
        // band runs before the query); nothing errors, nothing sends.
        assert!(env.queries.is_empty());
        assert!(!flag(&value_flag));
        assert!(!flag(&error_flag));
    }

    #[test]
    fn create_rich_message_with_ai_hits_the_rich_seam() {
        let mut env = TestTranslationEnv::new();
        let manager = TranslationManager::default();
        let ((_value_flag, _error_flag), promise) = FlagPromise::<td_api::RichMessage>::create();
        manager.create_rich_message_with_ai(
            &mut env,
            "draw a cat".to_owned(),
            "en".to_owned(),
            true,
            promise,
        );
        assert_eq!(env.queries, vec!["compose_rich"]);
    }

    #[test]
    fn proofread_rich_message_with_ai_hits_the_rich_seam() {
        let mut env = TestTranslationEnv::new();
        let manager = TranslationManager::default();
        let ((_value_flag, _error_flag), promise) = FlagPromise::<td_api::RichMessage>::create();
        manager.proofread_rich_message_with_ai(&mut env, None, promise);
        assert_eq!(env.queries, vec!["compose_rich"]);
    }

    #[test]
    fn on_get_ai_compose_tones_intake_persists_and_updates() {
        let mut env = TestTranslationEnv::new();
        let mut manager = TranslationManager::default();
        manager.on_get_ai_compose_tones(
            &mut env,
            telegram_api::aicompose_Tones::aicompose_tones(telegram_api::aicompose_tones {
                hash: 9,
                tones: vec![telegram_api::AiComposeTone::aiComposeTone(
                    telegram_api::aiComposeTone {
                        flags: 1,
                        creator: true,
                        id: 1001,
                        access_hash: 2002,
                        slug: "slg".to_owned(),
                        title: "T".to_owned(),
                        emoji_id: 77,
                        prompt: "p".to_owned(),
                        installs_count: 3,
                        author_id: 42,
                        example_english: None,
                    },
                )],
                users: Vec::new(),
            }),
        );
        // The answer re-intakes the authors, persists the pmc state, and
        // sends the styles update.
        assert_eq!(env.users_seen, 0);
        let blob = env
            .pmc
            .get(AI_COMPOSE_TONES_KEY)
            .cloned()
            .unwrap_or_default();
        assert!(!blob.is_empty());
        let mut parser = Parser::new(&blob);
        let stored = AiComposeTones::parse(&mut parser).unwrap();
        assert_eq!(stored.get_hash(), 9);
        assert_eq!(stored.tones.len(), 1);
        assert_eq!(env.updates.len(), 1);
        assert_eq!(styles_of(&env.updates[0]).len(), 1);

        // The identical answer is a no-op.
        manager.on_get_ai_compose_tones(
            &mut env,
            telegram_api::aicompose_Tones::aicompose_tones(telegram_api::aicompose_tones {
                hash: 9,
                tones: vec![telegram_api::AiComposeTone::aiComposeTone(
                    telegram_api::aiComposeTone {
                        flags: 1,
                        creator: true,
                        id: 1001,
                        access_hash: 2002,
                        slug: "slg".to_owned(),
                        title: "T".to_owned(),
                        emoji_id: 77,
                        prompt: "p".to_owned(),
                        installs_count: 3,
                        author_id: 42,
                        example_english: None,
                    },
                )],
                users: Vec::new(),
            }),
        );
        assert_eq!(env.updates.len(), 1);
    }

    #[test]
    fn create_tone_calls_get_me_then_parks_at_the_seam() {
        let mut env = TestTranslationEnv::new();
        let manager = TranslationManager::default();
        let ((_value_flag, _error_flag), promise) =
            FlagPromise::<td_api::TextCompositionStyle>::create();
        manager.create_tone(&mut env, "T".to_owned(), 77, "p".to_owned(), true, promise);
        assert_eq!(env.me_calls, 1);
        assert_eq!(env.queries, vec!["create_tone"]);
    }

    #[test]
    fn create_tone_after_close_errors_without_the_seam() {
        let mut env = TestTranslationEnv::new();
        env.close_flag = true;
        let manager = TranslationManager::default();
        let ((_value_flag, error_flag), promise) =
            FlagPromise::<td_api::TextCompositionStyle>::create();
        manager.create_tone(&mut env, "T".to_owned(), 77, "p".to_owned(), true, promise);
        assert_eq!(env.me_calls, 1);
        assert!(env.queries.is_empty());
        assert!(flag(&error_flag));
    }

    #[test]
    fn get_current_state_gates_on_authorized_and_bot() {
        let mut env = TestTranslationEnv::new();
        let manager = TranslationManager::default();
        let mut updates = Vec::new();
        manager.get_current_state(&mut env, &mut updates);
        assert_eq!(updates.len(), 1);
        env.bot = true;
        let mut updates = Vec::new();
        manager.get_current_state(&mut env, &mut updates);
        assert!(updates.is_empty());
        env.bot = false;
        env.authorized = false;
        let mut updates = Vec::new();
        manager.get_current_state(&mut env, &mut updates);
        assert!(updates.is_empty());
    }
}
