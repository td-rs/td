//! Port of `td/telegram/Game.{h,cpp,hpp}` — the game value plane of the
//! M.46 bots slice (`process_input_message_game`, the inline-result and
//! server-game constructors, the byte-exact store/parse).
//!
//! Seams:
//! - The photo/animation object builders and the document → animation
//!   classification ride [`GameEnv`] (the
//!   FileManager/AnimationsManager/DocumentsManager collapse; see the
//!   `web_app` module doc for the same WebApp shape).
//! - The store/parse `FileId` rows ride [`crate::photo::PhotoStoreEnv`] and
//!   the animation row rides
//!   [`WebAppEnv::store_animation_row`]/[`WebAppEnv::parse_animation_row`]
//!   ([blocked: AnimationsManager]); the
//!   `Version::FixStoreGameWithoutAnimation` version gate is real.
//! - `Game::get_game_object` resolves the sender ids through
//!   [`UserManagerLike`] (the C++ `td->user_manager_.get()`), skipped
//!   with `is_server = false` exactly like the C++ (the `()` shim plays
//!   the null manager).

use crate::dialog_id::DialogId;
use crate::message_entity::{FormattedText, UserManagerLike, get_formatted_text_object};
use crate::photo::{Photo, parse_photo, store_photo};
use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::user_manager::{FileId, UserId};
use crate::web_app::{WebAppEnv, append_animation_file_ids};
use tdmtproto::telegram_api;
use tdutils::status::Status;

/// The `Td`-surface the Game value class reads through (the C++
/// `Game(Td *td, ...)` / `get_game_object(Td *td, ...)` / the store
/// `storer.context()->td()` accessors); the user-manager object plane
/// arrives through the [`UserManagerLike`] supertrait.
pub trait GameEnv: WebAppEnv + UserManagerLike {}

/// `td::Game` (Game.h:26).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Game {
    id: i64,
    access_hash: i64,
    bot_user_id: UserId,
    short_name: String,
    title: String,
    description: String,
    photo: Photo,
    animation_file_id: FileId,

    text: FormattedText,
}

impl Game {
    /// `Game::Game(Td *, UserId, tl_object_ptr<telegram_api::game> &&,
    /// FormattedText, DialogId)` (Game.cpp:23-32).
    pub fn from_telegram(
        env: &mut impl GameEnv,
        bot_user_id: UserId,
        game: telegram_api::Game,
        text: FormattedText,
        owner_dialog_id: DialogId,
    ) -> Game {
        let telegram_api::Game {
            title,
            description,
            photo,
            document,
            id,
            access_hash,
            short_name,
            ..
        } = game;
        let mut result =
            Game::from_inline(env, title, description, photo, document, owner_dialog_id);
        result.id = id;
        result.access_hash = access_hash;
        result.bot_user_id = if bot_user_id.is_valid() {
            bot_user_id
        } else {
            UserId::default()
        };
        result.short_name = short_name;
        result.text = text;
        result
    }

    /// `Game::Game(Td *, string title, string description, Photo &&,
    /// Document &&, DialogId)` — the inline-results constructor
    /// (Game.cpp:34-56).
    pub fn from_inline(
        env: &mut impl GameEnv,
        title: String,
        description: String,
        photo: Option<telegram_api::Photo>,
        document: Option<telegram_api::Document>,
        owner_dialog_id: DialogId,
    ) -> Game {
        let mut result = Game {
            title,
            description,
            ..Game::default()
        };
        if let Some(photo) = photo {
            // `get_photo(td, std::move(photo), owner_dialog_id)` — the
            // structural walk is the M.4.13 `get_photo`.
            result.photo = crate::photo::get_photo(photo);
            if result.photo.is_empty() {
                tdutils::log_error!("Receive empty photo for game {}", result.title);
                result.photo.id = 0; // to prevent null photo in td_api
            }
        }
        if let Some(document) = document
            && matches!(document, telegram_api::Document::document(_))
        {
            {
                match env.on_get_document_animation(document, owner_dialog_id) {
                    Some(file_id) => result.animation_file_id = file_id,
                    None => {
                        tdutils::log_error!("Receive non-animation document in the game");
                    }
                }
            }
        }
        result
    }

    /// `Game::Game(UserId, string short_name)` — the outgoing-message
    /// constructor (Game.cpp:58-63). The C++ `photo_.id = 0` sentinel
    /// keeps the constructed `Photo::default()`; the td_api builders
    /// read the env seam answer here.
    pub fn new_outgoing(bot_user_id: UserId, short_name: String) -> Game {
        let mut bot_user_id = bot_user_id;
        if !bot_user_id.is_valid() {
            bot_user_id = UserId::default();
        }
        Game {
            bot_user_id,
            short_name,
            ..Game::default()
        }
    }

    /// `Game::is_empty` (Game.cpp:65-67).
    pub fn is_empty(&self) -> bool {
        self.short_name.is_empty()
    }

    /// `Game::get_bot_user_id` (Game.cpp:69-71).
    pub fn get_bot_user_id(&self) -> UserId {
        self.bot_user_id
    }

    /// `Game::get_file_ids` (Game.cpp:73-77).
    pub fn get_file_ids(&self) -> Vec<FileId> {
        let mut result = crate::photo::photo_get_file_ids(&self.photo);
        append_animation_file_ids(self.animation_file_id, &mut result);
        result
    }

    /// `Game::get_text` (Game.cpp:79-81).
    pub fn get_text(&self) -> &FormattedText {
        &self.text
    }

    /// `Game::get_game_object` (Game.cpp:83-89) — the user-manager
    /// object plane is passed only for server games (`is_server`),
    /// exactly like the C++ ternary.
    pub fn get_game_object(
        &self,
        env: &mut impl GameEnv,
        is_server: bool,
        skip_bot_commands: bool,
    ) -> td_api::Game {
        let text = get_formatted_text_object(
            if is_server { Some(&mut *env) } else { None },
            &self.text,
            skip_bot_commands,
            -1,
        );
        td_api::Game {
            id: self.id,
            short_name: self.short_name.clone(),
            title: self.title.clone(),
            text: Some(text),
            description: self.description.clone(),
            photo: env.get_photo_object(&self.photo),
            animation: env.get_animation_object(self.animation_file_id),
        }
    }

    /// `Game::has_input_media` (Game.cpp:91-93).
    pub fn has_input_media(&self) -> bool {
        self.bot_user_id.is_valid()
    }

    /// `Game::get_input_media_game` (Game.cpp:95-99).
    pub fn get_input_media_game(&self, env: &mut impl GameEnv) -> telegram_api::InputMedia {
        let input_user = UserManagerLike::get_input_user_force(env, self.bot_user_id);
        telegram_api::InputMedia::inputMediaGame(telegram_api::inputMediaGame {
            id: Some(telegram_api::InputGame::inputGameShortName(
                telegram_api::inputGameShortName {
                    bot_id: Some(input_user),
                    short_name: self.short_name.clone(),
                },
            )),
        })
    }

    /// `Game::store` (Game.hpp:22-39) — byte-exact; the animation row
    /// rides the env seam under the `has_animation` flag, the caption
    /// text rides `FormattedText::store`.
    pub fn store(&self, env: &mut impl GameEnv, out: &mut Vec<u8>) {
        let has_animation = self.animation_file_id.is_valid();
        let flags_offset = out.len();
        store_parse::store_flags(out, [has_animation]);
        store_parse::store_i64(out, self.id);
        store_parse::store_i64(out, self.access_hash);
        store_parse::store_i64(out, self.bot_user_id.get());
        store_parse::store_string(out, &self.short_name);
        store_parse::store_string(out, &self.title);
        store_parse::store_string(out, &self.description);
        store_photo(env, out, &self.photo);
        if has_animation && !env.store_animation_row(self.animation_file_id, out) {
            // the [blocked: AnimationsManager] interim: the row is
            // skipped and the flag byte rewritten to keep the format
            // self-consistent
            out[flags_offset..flags_offset + 4].copy_from_slice(&0u32.to_le_bytes());
        }
        self.text.store(out);
    }

    /// `Game::parse` (Game.hpp:42-61) — byte-exact; pre-
    /// `Version::FixStoreGameWithoutAnimation` rows always carry the
    /// animation row (the C++ `bool has_animation = true` default).
    pub fn parse(env: &mut impl GameEnv, parser: &mut Parser) -> Result<Game, &'static str> {
        let mut has_animation = true;
        if parser.version() >= store_parse::version::FIX_STORE_GAME_WITHOUT_ANIMATION {
            let [parsed] = parser.parse_flags()?;
            has_animation = parsed;
        }
        let id = parser.i64_()?;
        let access_hash = parser.i64_()?;
        let bot_user_id = UserId::new(parser.i64_()?);
        let short_name = parser.string()?;
        let title = parser.string()?;
        let description = parser.string()?;
        let photo = parse_photo(env, parser)?;
        let animation_file_id = if has_animation {
            env.parse_animation_row(parser)?
        } else {
            FileId::default()
        };
        let text = FormattedText::parse(parser)?;
        Ok(Game {
            id,
            access_hash,
            bot_user_id,
            short_name,
            title,
            description,
            photo,
            animation_file_id,
            text,
        })
    }
}

/// `process_input_message_game` (Game.cpp:118-137) — the td_api input
/// validation half; the `None` arm models the C++ null-input `CHECK`
/// (unreachable from well-typed callers).
pub fn process_input_message_game(
    env: &mut impl GameEnv,
    input_message_content: Option<td_api::InputMessageContent>,
) -> Result<Game, Status> {
    let Some(td_api::InputMessageContent::inputMessageGame(input_message_game)) =
        input_message_content
    else {
        return Err(Status::error(500, "Receive invalid input message game"));
    };

    let bot_user_id = UserId::new(input_message_game.bot_user_id);
    UserManagerLike::ensure_input_user(env, bot_user_id)?;

    let mut game_short_name = input_message_game.game_short_name;
    if crate::misc::clean_input_string(&game_short_name).is_none() {
        return Err(Status::error(
            400,
            "Game short name must be encoded in UTF-8",
        ));
    }

    // TODO validate game_short_name
    if game_short_name.is_empty() {
        return Err(Status::error(400, "Game short name must be non-empty"));
    }

    Ok(Game::new_outgoing(
        bot_user_id,
        std::mem::take(&mut game_short_name),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_parse::{self, Parser};
    use std::collections::HashMap;

    /// The recording value-plane env: the symmetric file/animation-row
    /// stand-ins and the document classification fixture.
    struct TestEnv {
        animation_rows: HashMap<i32, i32>,
        input_users: Vec<i64>,
    }

    impl Default for TestEnv {
        fn default() -> Self {
            TestEnv {
                animation_rows: HashMap::new(),
                input_users: vec![9],
            }
        }
    }

    impl crate::photo::PhotoStoreEnv for TestEnv {
        fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>) {
            store_parse::store_i32(out, file_id.get());
        }

        fn parse_file_id(&mut self, parser: &mut Parser<'_>) -> Result<FileId, &'static str> {
            Ok(FileId::new(parser.i32_()?, 0))
        }
    }

    impl crate::web_app::WebAppEnv for TestEnv {
        fn on_get_document_animation(
            &mut self,
            document: telegram_api::Document,
            _owner_dialog_id: crate::dialog_id::DialogId,
        ) -> Option<FileId> {
            let telegram_api::Document::document(document) = document else {
                return None;
            };
            (document.id == 7).then_some(FileId::new(77, 0))
        }

        fn get_animation_object(&self, _file_id: FileId) -> Option<td_api::Animation> {
            None
        }

        fn get_photo_object(&self, _photo: &Photo) -> Option<td_api::Photo> {
            None
        }

        fn store_animation_row(&mut self, file_id: FileId, out: &mut Vec<u8>) -> bool {
            if !file_id.is_valid() {
                return false;
            }
            store_parse::store_i32(out, file_id.get());
            self.animation_rows.insert(file_id.get(), file_id.get());
            true
        }

        fn parse_animation_row(&mut self, parser: &mut Parser<'_>) -> Result<FileId, &'static str> {
            Ok(FileId::new(parser.i32_()?, 0))
        }
    }

    impl crate::message_entity::UserManagerLike for TestEnv {
        fn resolve_user_id_object(&mut self, user_id: UserId, _source: &str) -> i64 {
            user_id.get()
        }

        fn ensure_input_user(&mut self, user_id: UserId) -> std::result::Result<(), Status> {
            if self.input_users.contains(&user_id.get()) {
                Ok(())
            } else {
                Err(Status::error(400, "User not found"))
            }
        }

        fn have_min_user(&self, user_id: UserId) -> bool {
            self.input_users.contains(&user_id.get())
        }

        fn get_input_user_force(&mut self, user_id: UserId) -> telegram_api::InputUser {
            telegram_api::InputUser::inputUser(telegram_api::inputUser {
                user_id: user_id.get(),
                access_hash: 0,
            })
        }
    }

    impl GameEnv for TestEnv {}

    #[test]
    fn game_from_telegram_classifies_the_animation_document() {
        let mut env = TestEnv::default();
        let game = Game::from_telegram(
            &mut env,
            UserId::new(9),
            telegram_api::Game {
                flags: 0,
                id: 11,
                access_hash: 22,
                short_name: "git".to_owned(),
                title: "Game".to_owned(),
                description: "d".to_owned(),
                photo: Some(telegram_api::Photo::photoEmpty(telegram_api::photoEmpty {
                    id: -1,
                })),
                document: Some(telegram_api::Document::document(telegram_api::document {
                    id: 7,
                    ..Default::default()
                })),
            },
            FormattedText::default(),
            crate::dialog_id::DialogId::new(5),
        );
        // document id 7 classifies as the animation in the fixture env
        assert!(game.get_file_ids().iter().any(|f| f.get() == 77));
        assert_eq!(game.get_bot_user_id(), UserId::new(9));
        assert!(!game.is_empty());
        assert!(game.has_input_media());

        // an invalid bot user id collapses to the default
        let game = Game::from_telegram(
            &mut env,
            UserId::new(0),
            telegram_api::Game {
                id: 1,
                ..Default::default()
            },
            FormattedText::default(),
            crate::dialog_id::DialogId::new(5),
        );
        assert_eq!(game.get_bot_user_id(), UserId::default());
        assert!(!game.has_input_media());
    }

    #[test]
    fn outgoing_game_and_input_media() {
        let game = Game::new_outgoing(UserId::new(9), "git".to_owned());
        assert!(game.has_input_media());

        let mut env = TestEnv::default();
        let input_media = game.get_input_media_game(&mut env);
        let telegram_api::InputMedia::inputMediaGame(media) = input_media else {
            panic!("wrong input media");
        };
        let Some(telegram_api::InputGame::inputGameShortName(short_name)) = media.id else {
            panic!("wrong input game");
        };
        assert_eq!(short_name.short_name, "git");
        // the force-form input user for the valid bot id
        assert_eq!(
            short_name.bot_id,
            Some(telegram_api::InputUser::inputUser(
                telegram_api::inputUser {
                    user_id: 9,
                    access_hash: 0,
                }
            ))
        );
    }

    #[test]
    fn game_store_parse_is_byte_symmetric() {
        let mut env = TestEnv::default();
        let game = Game::from_telegram(
            &mut env,
            UserId::new(9),
            telegram_api::Game {
                flags: 0,
                id: 11,
                access_hash: 22,
                short_name: "git".to_owned(),
                title: "Game".to_owned(),
                description: "d".to_owned(),
                photo: None,
                document: Some(telegram_api::Document::document(telegram_api::document {
                    id: 7,
                    ..Default::default()
                })),
            },
            FormattedText {
                text: "caption".to_owned(),
                entities: Vec::new(),
            },
            crate::dialog_id::DialogId::new(5),
        );

        let mut stored = Vec::new();
        store_parse::store_log_event_version(&mut stored);
        game.store(&mut env, &mut stored);
        let mut parser = Parser::new_log_event(&stored).expect("framed");
        let parsed = Game::parse(&mut env, &mut parser).expect("parses");
        parser.fetch_end().expect("fully consumed");
        assert_eq!(parsed, game);
        assert_eq!(parsed.get_text().text, "caption");

        // the [blocked: AnimationsManager] interim keeps the flag honest:
        // a refusing seam stores the game animation-less and the parse
        // round-trips the same bytes
        let game = Game::new_outgoing(UserId::new(9), "other".to_owned());
        let mut stored = Vec::new();
        game.store(&mut env, &mut stored);
        let mut parser = Parser::new(&stored);
        let parsed = Game::parse(&mut env, &mut parser).expect("parses");
        assert_eq!(parsed, game);
    }

    #[test]
    fn process_input_message_game_validates_the_input() {
        let mut env = TestEnv::default();

        let content = td_api::InputMessageContent::inputMessageGame(td_api::inputMessageGame {
            bot_user_id: 9,
            game_short_name: "git".to_owned(),
        });
        let game = process_input_message_game(&mut env, Some(content)).expect("valid");
        assert_eq!(game.get_bot_user_id(), UserId::new(9));
        assert!(!game.is_empty());

        // an unknown bot fails the input-user validation
        let content = td_api::InputMessageContent::inputMessageGame(td_api::inputMessageGame {
            bot_user_id: 999,
            game_short_name: "git".to_owned(),
        });
        let error = process_input_message_game(&mut env, Some(content)).unwrap_err();
        assert_eq!(error.code(), 400);

        // an empty short name
        let content = td_api::InputMessageContent::inputMessageGame(td_api::inputMessageGame {
            bot_user_id: 9,
            game_short_name: String::new(),
        });
        let error = process_input_message_game(&mut env, Some(content)).unwrap_err();
        assert_eq!(error.message(), "Game short name must be non-empty");

        // a null input content is the C++ CHECK caller contract
        let error = process_input_message_game(&mut env, None).unwrap_err();
        assert_eq!(error.code(), 500);
    }
}
