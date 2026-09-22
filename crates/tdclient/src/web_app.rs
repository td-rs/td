//! Port of `td/telegram/WebApp.{h,cpp,hpp}` and
//! `td/telegram/WebAppOpenParameters.{h,cpp}` — the Mini App value plane
//! of the M.46 bots slice, plus the theme-parameters JSON serializer the
//! open parameters carry (ThemeManager.cpp:609-637; the M.41 module owns
//! the copy behind the synchronous getThemeParametersJsonString).
//!
//! Seams:
//! - The document → animation classification inside the constructor
//!   (`td->documents_manager_->on_get_document`, WebApp.cpp:37-44) rides
//!   [`WebAppEnv::on_get_document_animation`] — [blocked: Documents]; the
//!   default drops the document like the C++ non-animation branch.
//! - `get_photo_object(td->file_manager_.get(), photo)`
//!   (WebApp.cpp:60) and
//!   `td->animations_manager_->get_animation_object` (WebApp.cpp:61) are
//!   the [`WebAppEnv`] object seams (the FileManager/AnimationsManager
//!   object builders).
//! - The store/parse `FileId` rows ride [`PhotoStoreEnv`]
//!   (FileId.hpp:16-23) and the animation row rides
//!   [`WebAppEnv::store_animation_row`]/[`parse_animation_row`]
//!   (AnimationsManager.hpp:21-56, [blocked: AnimationsManager]).

use crate::photo::{Photo, PhotoStoreEnv, parse_photo, store_photo};
use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::user_manager::FileId;
use tdmtproto::telegram_api;
use tdutils::json::{JsonString, json_encode, json_object_fn};

/// The `Td`-surface the WebApp value class reads through (the C++
/// `WebApp(Td *td, ...)` / `get_web_app_object(Td *td)` / the store
/// `storer.context()->td()` accessors).
pub trait WebAppEnv: PhotoStoreEnv {
    /// `td->documents_manager_->on_get_document(...)` (WebApp.cpp:37-41) —
    /// `Some(file_id)` models the `Document::Type::Animation` hit,
    /// `None` the non-animation/invalid bail ([blocked: Documents]).
    fn on_get_document_animation(
        &mut self,
        document: telegram_api::Document,
        owner_dialog_id: crate::dialog_id::DialogId,
    ) -> Option<FileId> {
        let _ = (document, owner_dialog_id);
        tdutils::log_info!("DocumentsManager::on_get_document — [blocked: Documents]");
        None
    }

    /// `td->animations_manager_->get_animation_object(file_id)`
    /// (WebApp.cpp:61).
    fn get_animation_object(&self, file_id: FileId) -> Option<td_api::Animation> {
        let _ = file_id;
        tdutils::log_info!("AnimationsManager::get_animation_object — seam");
        None
    }

    /// `get_photo_object(td->file_manager_.get(), photo)` (WebApp.cpp:60)
    /// — the FileManager photo builder; `None` is the interim
    /// null-object answer (the notification-plane convention).
    fn get_photo_object(&self, photo: &Photo) -> Option<td_api::Photo> {
        let _ = photo;
        tdutils::log_info!("FileManager::get_photo_object — seam");
        None
    }

    /// `animations_manager_->store_animation(file_id, storer)`
    /// (AnimationsManager.hpp:21-43, [blocked: AnimationsManager]);
    /// `false` models the unresolvable row (the value then stores
    /// animation-less, the flag stays consistent through the parse).
    fn store_animation_row(&mut self, file_id: FileId, out: &mut Vec<u8>) -> bool {
        let _ = (file_id, out);
        tdutils::log_info!("AnimationsManager::store_animation — [blocked: AnimationsManager]");
        false
    }

    /// `animations_manager_->parse_animation(parser)`
    /// (AnimationsManager.hpp:45-70).
    fn parse_animation_row(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
        let _ = parser;
        tdutils::log_info!("AnimationsManager::parse_animation — [blocked: AnimationsManager]");
        Result::Ok(FileId::default())
    }
}

/// `Document(Document::Type::Animation, animation_file_id).append_file_ids(
/// td, result)` (Game.cpp:75 / WebApp.cpp:54) — the animation file id
/// itself; the animated-thumbnail walk is the [blocked: Documents] leg.
pub(crate) fn append_animation_file_ids(animation_file_id: FileId, result: &mut Vec<FileId>) {
    if animation_file_id.is_valid() {
        result.push(animation_file_id);
    }
}

/// `td::WebApp` (WebApp.h:22).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebApp {
    id: i64,
    access_hash: i64,
    short_name: String,
    title: String,
    description: String,
    photo: Photo,
    animation_file_id: FileId,
    hash: i64,
}

impl WebApp {
    /// `WebApp::WebApp(Td *, telegram_api::object_ptr<botApp> &&, DialogId)`
    /// (WebApp.cpp:21-46).
    pub fn from_telegram(
        env: &mut impl WebAppEnv,
        web_app: telegram_api::botApp,
        owner_dialog_id: crate::dialog_id::DialogId,
    ) -> WebApp {
        let telegram_api::botApp {
            id,
            access_hash,
            short_name,
            title,
            description,
            photo,
            document,
            hash,
            ..
        } = web_app;
        let mut result = WebApp {
            id,
            access_hash,
            short_name,
            title,
            description,
            hash,
            ..WebApp::default()
        };
        if let Some(photo) = photo {
            // `get_photo(td, std::move(web_app->photo_), owner_dialog_id)`
            // — the file-registration leg rides the file plane, the
            // structural walk is the M.4.13 `get_photo`.
            result.photo = crate::photo::get_photo(photo);
            if result.photo.is_empty() {
                tdutils::log_error!(
                    "Receive empty photo for Web App {}/{}",
                    result.short_name,
                    result.title
                );
                result.photo.id = 0; // to prevent null photo in td_api
            }
        }
        // `document->get_id() == telegram_api::document::ID` — the
        // umbrella enum carries only the `document` arm on this layer.
        if let Some(document) = document
            && matches!(document, telegram_api::Document::document(_))
        {
            match env.on_get_document_animation(document, owner_dialog_id) {
                Some(file_id) => result.animation_file_id = file_id,
                None => {
                    tdutils::log_error!(
                        "Receive non-animation document for Web App {}/{}",
                        result.short_name,
                        result.title
                    );
                }
            }
        }
        result
    }

    /// `WebApp::is_empty` (WebApp.cpp:48-50).
    pub fn is_empty(&self) -> bool {
        self.short_name.is_empty()
    }

    /// `WebApp::get_file_ids` (WebApp.cpp:52-56).
    pub fn get_file_ids(&self) -> Vec<FileId> {
        let mut result = crate::photo::photo_get_file_ids(&self.photo);
        append_animation_file_ids(self.animation_file_id, &mut result);
        result
    }

    /// `WebApp::get_web_app_object` (WebApp.cpp:58-62).
    pub fn get_web_app_object(&self, env: &mut impl WebAppEnv) -> td_api::WebApp {
        td_api::WebApp {
            short_name: self.short_name.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            photo: env.get_photo_object(&self.photo),
            animation: env.get_animation_object(self.animation_file_id),
        }
    }

    /// `WebApp::store` (WebApp.hpp:21-37) — byte-exact; the animation row
    /// is kept under the same `has_animation` flag through the env seam.
    pub fn store(&self, env: &mut impl WebAppEnv, out: &mut Vec<u8>) {
        let has_animation = self.animation_file_id.is_valid();
        let flags_offset = out.len();
        store_parse::store_flags(out, [has_animation]);
        store_parse::store_i64(out, self.id);
        store_parse::store_i64(out, self.access_hash);
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
        store_parse::store_i64(out, self.hash);
    }

    /// `WebApp::parse` (WebApp.hpp:40-56) — byte-exact.
    pub fn parse(env: &mut impl WebAppEnv, parser: &mut Parser) -> Result<WebApp, &'static str> {
        let [has_animation] = parser.parse_flags()?;
        let id = parser.i64_()?;
        let access_hash = parser.i64_()?;
        let short_name = parser.string()?;
        let title = parser.string()?;
        let description = parser.string()?;
        let photo = parse_photo(env, parser)?;
        let animation_file_id = if has_animation {
            env.parse_animation_row(parser)?
        } else {
            FileId::default()
        };
        let hash = parser.i64_()?;
        Ok(WebApp {
            id,
            access_hash,
            short_name,
            title,
            description,
            photo,
            animation_file_id,
            hash,
        })
    }
}

/// `td::WebAppOpenParameters` (WebAppOpenParameters.h:16).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebAppOpenParameters {
    theme_parameters: Option<td_api::ThemeParameters>,
    application_name: String,
    is_compact: bool,
    is_full_screen: bool,
}

impl WebAppOpenParameters {
    /// `WebAppOpenParameters::WebAppOpenParameters(td_api::object_ptr<
    /// td_api::webAppOpenParameters> &&)` (WebAppOpenParameters.cpp:14-36).
    pub fn from_td_api(parameters: Option<td_api::WebAppOpenParameters>) -> WebAppOpenParameters {
        let Some(parameters) = parameters else {
            return WebAppOpenParameters::default();
        };
        let mut result = WebAppOpenParameters {
            theme_parameters: parameters.theme,
            application_name: parameters.application_name,
            is_compact: false,
            is_full_screen: false,
        };
        // the C++ cleans in place and only clears on an invalid UTF-8
        match crate::misc::clean_input_string(&result.application_name) {
            Some(cleaned) => result.application_name = cleaned,
            None => result.application_name.clear(),
        }
        if let Some(mode) = parameters.mode {
            match mode {
                td_api::WebAppOpenMode::webAppOpenModeCompact(_) => result.is_compact = true,
                td_api::WebAppOpenMode::webAppOpenModeFullSize(_) => {}
                td_api::WebAppOpenMode::webAppOpenModeFullScreen(_) => result.is_full_screen = true,
            }
        }
        result
    }

    /// `WebAppOpenParameters::get_input_theme_parameters`
    /// (WebAppOpenParameters.cpp:38-44) — the `dataJSON` wrapper over the
    /// theme-parameters JSON string.
    pub fn get_input_theme_parameters(&self) -> Option<telegram_api::DataJSON> {
        let theme_parameters = self.theme_parameters.as_ref()?;
        Some(telegram_api::DataJSON {
            data: get_theme_parameters_json_string(theme_parameters),
        })
    }

    /// `WebAppOpenParameters::get_application_name`
    /// (WebAppOpenParameters.h:27-29).
    pub fn get_application_name(&self) -> &str {
        &self.application_name
    }

    /// `WebAppOpenParameters::is_compact` (WebAppOpenParameters.h:31-33).
    pub fn is_compact(&self) -> bool {
        self.is_compact
    }

    /// `WebAppOpenParameters::is_full_screen`
    /// (WebAppOpenParameters.h:35-37).
    pub fn is_full_screen(&self) -> bool {
        self.is_full_screen
    }
}

/// `ThemeManager::get_theme_parameters_json_string` (ThemeManager.cpp:609-637)
/// — the JSON object keeps the C++ insertion order and the `#rrggbb`
/// lowercase color form (the copy the M.41 module carries behind the
/// synchronous getThemeParametersJsonString).
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
    let theme_parameters = theme_parameters.clone();
    json_encode(
        &json_object_fn(move |o| {
            o.field(
                "bg_color",
                &JsonString(&get_color(theme_parameters.background_color)),
            );
            o.field(
                "secondary_bg_color",
                &JsonString(&get_color(theme_parameters.secondary_background_color)),
            );
            o.field(
                "text_color",
                &JsonString(&get_color(theme_parameters.text_color)),
            );
            o.field(
                "hint_color",
                &JsonString(&get_color(theme_parameters.hint_color)),
            );
            o.field(
                "link_color",
                &JsonString(&get_color(theme_parameters.link_color)),
            );
            o.field(
                "button_color",
                &JsonString(&get_color(theme_parameters.button_color)),
            );
            o.field(
                "button_text_color",
                &JsonString(&get_color(theme_parameters.button_text_color)),
            );
            o.field(
                "header_bg_color",
                &JsonString(&get_color(theme_parameters.header_background_color)),
            );
            o.field(
                "bottom_bar_bg_color",
                &JsonString(&get_color(theme_parameters.bottom_bar_background_color)),
            );
            o.field(
                "section_bg_color",
                &JsonString(&get_color(theme_parameters.section_background_color)),
            );
            o.field(
                "section_separator_color",
                &JsonString(&get_color(theme_parameters.section_separator_color)),
            );
            o.field(
                "accent_text_color",
                &JsonString(&get_color(theme_parameters.accent_text_color)),
            );
            o.field(
                "section_header_text_color",
                &JsonString(&get_color(theme_parameters.section_header_text_color)),
            );
            o.field(
                "subtitle_text_color",
                &JsonString(&get_color(theme_parameters.subtitle_text_color)),
            );
            o.field(
                "destructive_text_color",
                &JsonString(&get_color(theme_parameters.destructive_text_color)),
            );
        }),
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_parse::{self, Parser};
    use std::collections::HashMap;

    /// The recording value-plane env: the symmetric file/animation-row
    /// stand-ins and the document classification fixture.
    #[derive(Default)]
    struct TestEnv {
        animation_rows: HashMap<i32, i32>,
    }

    impl crate::photo::PhotoStoreEnv for TestEnv {
        fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>) {
            store_parse::store_i32(out, file_id.get());
        }

        fn parse_file_id(&mut self, parser: &mut Parser<'_>) -> Result<FileId, &'static str> {
            Ok(FileId::new(parser.i32_()?, 0))
        }
    }

    impl WebAppEnv for TestEnv {
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

    #[test]
    fn web_app_value_from_telegram_and_object() {
        let mut env = TestEnv::default();
        let web_app = WebApp::from_telegram(
            &mut env,
            telegram_api::botApp {
                flags: 0,
                id: 100,
                access_hash: 200,
                short_name: "app".to_owned(),
                title: "App".to_owned(),
                description: "d".to_owned(),
                photo: None,
                document: Some(telegram_api::Document::document(telegram_api::document {
                    id: 7,
                    ..Default::default()
                })),
                hash: 300,
            },
            crate::dialog_id::DialogId::new(5),
        );
        assert!(!web_app.is_empty());
        assert!(web_app.get_file_ids().iter().any(|f| f.get() == 77));

        let object = web_app.get_web_app_object(&mut env);
        assert_eq!(object.short_name, "app");
        assert!(object.photo.is_none());

        // a document the seam cannot classify is dropped like the C++
        // non-animation branch
        let no_animation = WebApp::from_telegram(
            &mut env,
            telegram_api::botApp {
                flags: 0,
                short_name: "b".to_owned(),
                document: Some(telegram_api::Document::document(telegram_api::document {
                    id: 8,
                    ..Default::default()
                })),
                ..Default::default()
            },
            crate::dialog_id::DialogId::new(5),
        );
        assert!(no_animation.get_file_ids().is_empty());
    }

    #[test]
    fn web_app_store_parse_is_byte_symmetric() {
        let mut env = TestEnv::default();
        let web_app = WebApp::from_telegram(
            &mut env,
            telegram_api::botApp {
                flags: 0,
                id: 100,
                access_hash: 200,
                short_name: "app".to_owned(),
                title: "App".to_owned(),
                description: "d".to_owned(),
                photo: None,
                document: Some(telegram_api::Document::document(telegram_api::document {
                    id: 7,
                    ..Default::default()
                })),
                hash: 300,
            },
            crate::dialog_id::DialogId::new(5),
        );

        let mut stored = Vec::new();
        store_parse::store_log_event_version(&mut stored);
        web_app.store(&mut env, &mut stored);
        let mut parser = Parser::new_log_event(&stored).expect("framed");
        let parsed = WebApp::parse(&mut env, &mut parser).expect("parses");
        parser.fetch_end().expect("fully consumed");
        assert_eq!(parsed, web_app);
    }

    #[test]
    fn get_theme_parameters_json_string_orders_the_fields() {
        let theme = td_api::ThemeParameters {
            background_color: 0xFF_FF_FF,
            secondary_background_color: 0x00_00_00,
            text_color: 0x0A_BC_DE,
            ..Default::default()
        };
        let json = get_theme_parameters_json_string(&theme);
        // the C++ insertion order and the lowercase #rrggbb form
        assert!(json.starts_with("{\"bg_color\":\"#ffffff\",\"secondary_bg_color\":\"#000000\",\"text_color\":\"#0abcde\","));
        assert!(json.ends_with("\"destructive_text_color\":\"#000000\"}"));
        assert_eq!(json.matches("\"#").count(), 15);
    }
}
