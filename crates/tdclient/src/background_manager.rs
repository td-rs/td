//! `BackgroundManager` (td/telegram/BackgroundManager.{h,cpp}) — the
//! backgrounds slice (the REMAINING-queue "BackgroundManager — 7 arms"
//! item; the theme plane's "[blocked: M.16 Backgrounds]" seams): the
//! installed/selected backgrounds registry, the wallpaper intake
//! (`on_get_background`), the local-background id machine, the upload
//! flow, and the nine request arms.
//!
//! The value plane (`BackgroundType`/`BackgroundFill`) and the
//! `BackgroundId` wrapper landed with M.41 in [`crate::theme_manager`] —
//! this slice owns the link/input/local-name halves there recorded as
//! "the Backgrounds-track slice", and takes over `BackgroundId` as its
//! canonical home (theme_manager and files re-export it).
//!
//! Seams:
//! - The six `Td::ResultHandler` queries (`GetBackgroundsQuery`,
//!   `GetBackgroundQuery`, `InstallBackgroundQuery`, `UploadBackgroundQuery`,
//!   `UnsaveBackgroundQuery`, `ResetBackgroundsQuery`) and the
//!   `SetChatWallPaperQuery` ride the `BackgroundNetKind` plane (net-drain
//!   33) — the whole-Td env send arms dispatch the wire queries and the
//!   answers drain through `Td::on_background_net_answer` into the landed
//!   handlers (`on_get_backgrounds`, `on_installed_background`,
//!   `on_uploaded_background_file`, `on_removed_background`,
//!   `on_reset_background`); the trait defaults keep the `[net-inert]`
//!   posture for the hermetic tests.
//! - `FileManager::upload` + the `UploadBackgroundFileCallback` hop ride
//!   [`BackgroundManagerEnv::start_background_file_upload`] — the collapsed
//!   line has no upload-completion pump yet (the M.12 transfer loop parks
//!   without the net), so the callback never fires; `on_upload_background_file`
//!   / `on_upload_background_file_error` are real and unit-tested. The
//!   future wiring also grows the resolved `InputFile` delivery (the Rust
//!   `UploadCallback` collapse carries only the id, files.rs:15594-15598).
//!   The `account.uploadWallPaper` wire half past that hop is landed (the
//!   `UploadBackground` kind); the file-source delivery rides the Files
//!   plane landing.
//! - The C++ `FileSourceBackground` repair (`FileReferenceManager.cpp:533`
//!   → `BackgroundManager::reload_background`) is the
//!   [`BackgroundManager::reload_background`] public entry; the Rust
//!   files.rs repair ladder does not call it yet.
//! - The `SetChatWallPaperQuery` wire send/answer rides the
//!   [`BackgroundManagerEnv::send_set_chat_wallpaper_query`] override (the
//!   input-peer send leg, the flags, the `UpdatesManager::on_get_updates`
//!   result routing and the `WALLPAPER_NOT_FOUND` revert retry);
//!   `MessagesManager::on_update_dialog_background` (the `is_remove_`
//!   halves, cpp:123-125/154-156) and `DialogManager::reload_dialog_info_full`
//!   (the remove-error head, cpp:162) do not exist in Rust yet — those two
//!   bands log the seam until the dialog-inventory plane lands.
//! - `ThemeManager`'s four background seams (`ThemeManagerEnv`::
//!   `on_get_theme_background`/`get_theme_background_object`/
//!   `store_background_row`/`parse_background_row`) route to this manager in
//!   td.rs's `ThemeEnv` — the pre-landing collapses (the inline
//!   `register_remote` wallPaper half, the background-less theme row) retire.
//!
//! Recorded divergences:
//! - The C++ `searchBackground` sqlite-cached path resolves the
//!   `SearchBackgroundRequest` promise with the actor's captured empty
//!   `{BackgroundId, BackgroundType}` pair on the FIRST search (the async
//!   `get` answers after `do_run` returned), so the C++ answers 404
//!   "Not Found" and only the second search hits the name cache. The
//!   Rust collapsed line keeps the same shape: the db-load path resolves
//!   the promise with `None` (the 404 in the arm). The `RequestActor`
//!   `set_tries(3)` ladder rides the net answer (the `load_user(id, tries)`
//!   precedent): a null `account.getWallPaper` answer re-sends up to three
//!   wire queries, then answers the tries-exhausted 500 — where the C++
//!   errors at the third park, the collapse consumes the third answer
//!   first (same wire traffic, same terminal error).
//! - The search/install/upload promises carry the `td_api::Background`
//!   answer directly (`None` models the C++ `send_result(nullptr)` 404)
//!   instead of the C++ actor re-reading its captured members after a
//!   `Unit` promise — the collapsed-actor shape.
//! - The C++ sqlite pmc `get` is async with a `being_loaded_from_database`
//!   promise queue; the Rust collapsed line inlines the callback (the
//!   common-kv get is synchronous — the AnimationsManagerEnv precedent),
//!   so the queue never accumulates.
//! - The `UploadBackgroundQuery` handler carries its promise in the C++;
//!   the Rust manager keeps the in-flight upload continuation in
//!   `pending_upload_queries` until the seam answer arrives (the
//!   collapsed-actor equivalent).
//! - `MessageDbStorageBridge` (messages_manager.rs:9271) still stores the
//!   embedded message background row as the bare id (the recorded raw-id
//!   bridge divergence); the real `Background` row store/parse below is
//!   byte-exact against `Background::store`/`parse` and covers the pmc
//!   keys (`bg`/`bgd`/`bgs`/`bgsd`/`max_bg_id`/`bgn*`).
//! - Three `BackgroundId` mirrors existed (theme_manager, files,
//!   message_content); this module takes over the canonical one
//!   (theme_manager and files re-export it), the message_content mirror
//!   stays (the `MessageContentStorage` raw-id seam).

use crate::chat_manager::{ChannelId, DialogParticipantStatus};
use crate::dialog_id::{DialogId, DialogType};
use crate::files::{FileId, FileSourceId, FileType, FileUploadId};
use crate::messages_manager::MessageId;
use crate::photo::PhotoFormat;
use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::theme_manager::{
    BackgroundFill, BackgroundFillType, BackgroundType, BackgroundTypeType,
};
use crate::user_manager::{AccessRights, SecretChatId, UserId};
use std::collections::{HashMap, HashSet};
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::{Result as StatusResult, Status};

/// `td::BackgroundId` (BackgroundId.h) — the canonical id wrapper; the
/// theme plane re-exports it and files.rs's wire mirror now points here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BackgroundId(i64);

impl BackgroundId {
    pub fn new(id: i64) -> BackgroundId {
        BackgroundId(id)
    }

    /// `BackgroundId::is_valid` — `id != 0`.
    pub fn is_valid(self) -> bool {
        self.0 != 0
    }

    /// `BackgroundId::is_local` — `0 < id <= 0x7FFFFFFF`.
    pub fn is_local(self) -> bool {
        0 < self.0 && self.0 <= 0x7FFF_FFFF
    }

    pub fn get(self) -> i64 {
        self.0
    }

    /// `BackgroundId::store` (BackgroundId.h:49-52).
    pub fn store(self, out: &mut Vec<u8>) {
        store_parse::store_i64(out, self.0);
    }

    /// `BackgroundId::parse` (BackgroundId.h:54-57).
    pub fn parse(parser: &mut Parser<'_>) -> std::result::Result<BackgroundId, &'static str> {
        Ok(BackgroundId::new(parser.i64_()?))
    }
}

/// `get_color_hex_string` (BackgroundType.cpp:20-26).
fn get_color_hex_string(color: i32) -> String {
    let mut result = String::with_capacity(6);
    for shift in [20, 16, 12, 8, 4, 0] {
        let digit = ((color >> shift) & 0xF) as usize;
        result.push(b"0123456789abcdef"[digit] as char);
    }
    result
}

/// `is_valid_rotation_angle` (BackgroundType.cpp:37-39).
fn is_valid_rotation_angle(rotation_angle: i32) -> bool {
    (0..360).contains(&rotation_angle) && rotation_angle % 45 == 0
}

/// `is_valid_intensity` (BackgroundType.cpp:41-43).
fn is_valid_intensity(intensity: i32, allow_negative: bool) -> bool {
    let min = if allow_negative { -100 } else { 0 };
    (min..=100).contains(&intensity)
}

/// `is_valid_color` (misc.cpp:430-432).
fn is_valid_color(color: i32) -> bool {
    (0..=0xFF_FFFF).contains(&color)
}

// ==== the Backgrounds-track value-plane halves (BackgroundType.cpp) ====

impl BackgroundFill {
    /// `BackgroundFill(int32 solid_color)` (BackgroundType.h:27-29).
    fn solid(color: i32) -> BackgroundFill {
        BackgroundFill {
            top_color: color,
            bottom_color: color,
            ..BackgroundFill::default()
        }
    }

    /// `BackgroundFill(top, bottom, rotation_angle)` (BackgroundType.h:30-34).
    fn gradient(top_color: i32, bottom_color: i32, rotation_angle: i32) -> BackgroundFill {
        BackgroundFill {
            top_color,
            bottom_color,
            rotation_angle,
            ..BackgroundFill::default()
        }
    }

    /// `BackgroundFill(first..fourth)` (BackgroundType.h:35-37).
    fn freeform(
        first_color: i32,
        second_color: i32,
        third_color: i32,
        fourth_color: i32,
    ) -> BackgroundFill {
        BackgroundFill {
            top_color: first_color,
            bottom_color: second_color,
            third_color,
            fourth_color,
            ..BackgroundFill::default()
        }
    }

    /// `BackgroundFill::get_background_fill(const td_api::BackgroundFill *)`
    /// (BackgroundType.cpp:95-137).
    pub(crate) fn get_background_fill(
        fill: Option<&td_api::BackgroundFill>,
    ) -> std::result::Result<BackgroundFill, Status> {
        let Some(fill) = fill else {
            return std::result::Result::Err(Status::error(
                400,
                "Background fill info must be non-empty",
            ));
        };
        match fill {
            td_api::BackgroundFill::backgroundFillSolid(solid) => {
                if !is_valid_color(solid.color) {
                    return std::result::Result::Err(Status::error(
                        400,
                        "Invalid solid fill color value",
                    ));
                }
                std::result::Result::Ok(BackgroundFill::solid(solid.color))
            }
            td_api::BackgroundFill::backgroundFillGradient(gradient) => {
                if !is_valid_color(gradient.top_color) {
                    return std::result::Result::Err(Status::error(
                        400,
                        "Invalid top gradient color value",
                    ));
                }
                if !is_valid_color(gradient.bottom_color) {
                    return std::result::Result::Err(Status::error(
                        400,
                        "Invalid bottom gradient color value",
                    ));
                }
                if !is_valid_rotation_angle(gradient.rotation_angle) {
                    return std::result::Result::Err(Status::error(
                        400,
                        "Invalid rotation angle value",
                    ));
                }
                std::result::Result::Ok(BackgroundFill::gradient(
                    gradient.top_color,
                    gradient.bottom_color,
                    gradient.rotation_angle,
                ))
            }
            td_api::BackgroundFill::backgroundFillFreeformGradient(freeform) => {
                if freeform.colors.len() != 3 && freeform.colors.len() != 4 {
                    return std::result::Result::Err(Status::error(
                        400,
                        "Wrong number of gradient colors",
                    ));
                }
                for &color in &freeform.colors {
                    if !is_valid_color(color) {
                        return std::result::Result::Err(Status::error(
                            400,
                            "Invalid freeform gradient color value",
                        ));
                    }
                }
                std::result::Result::Ok(BackgroundFill::freeform(
                    freeform.colors[0],
                    freeform.colors[1],
                    freeform.colors[2],
                    if freeform.colors.len() == 3 {
                        -1
                    } else {
                        freeform.colors[3]
                    },
                ))
            }
        }
    }

    /// `BackgroundFill::get_background_fill(Slice name)`
    /// (BackgroundType.cpp:139-197).
    pub(crate) fn get_background_fill_from_name(
        name: &str,
    ) -> std::result::Result<BackgroundFill, Status> {
        let hash_pos = name.find('#').unwrap_or(name.len());
        let name = &name[..hash_pos];

        let (name, parameters) = match name.find('?') {
            Some(pos) => (&name[..pos], &name[pos + 1..]),
            None => (name, ""),
        };

        let get_color = |color_string: &str| -> std::result::Result<i32, Status> {
            if color_string.len() > 6 {
                return std::result::Result::Err(Status::error(400, "WALLPAPER_INVALID"));
            }
            let decoded = tdutils::http_url::url_decode(color_string, true);
            match u32::from_str_radix(&decoded, 16) {
                Ok(color) => std::result::Result::Ok(color as i32),
                Err(_) => std::result::Result::Err(Status::error(400, "WALLPAPER_INVALID")),
            }
        };

        let mut hyphen_pos = name.find('-').unwrap_or(name.len());
        if name.contains('~') {
            let parts: Vec<&str> = name.split('~').collect();
            assert!(parts.len() >= 2);
            if parts.len() == 2 {
                hyphen_pos = parts[0].len();
            } else {
                if parts.len() > 4 {
                    return std::result::Result::Err(Status::error(400, "WALLPAPER_INVALID"));
                }
                let first_color = get_color(parts[0])?;
                let second_color = get_color(parts[1])?;
                let third_color = get_color(parts[2])?;
                let mut fourth_color = -1;
                if parts.len() == 4 {
                    fourth_color = get_color(parts[3])?;
                }
                return std::result::Result::Ok(BackgroundFill::freeform(
                    first_color,
                    second_color,
                    third_color,
                    fourth_color,
                ));
            }
        }

        if hyphen_pos < name.len() {
            let top_color = get_color(&name[..hyphen_pos])?;
            let bottom_color = get_color(&name[hyphen_pos + 1..])?;
            let mut rotation_angle = 0;

            const PREFIX: &str = "rotation=";
            if let Some(rest) = parameters.strip_prefix(PREFIX) {
                rotation_angle = tdutils::http_url::url_decode(rest, true)
                    .parse::<i32>()
                    .unwrap_or(0);
                if !is_valid_rotation_angle(rotation_angle) {
                    rotation_angle = 0;
                }
            }

            return std::result::Result::Ok(BackgroundFill::gradient(
                top_color,
                bottom_color,
                rotation_angle,
            ));
        }

        std::result::Result::Ok(BackgroundFill::solid(get_color(name)?))
    }

    /// `BackgroundFill::get_link` (BackgroundType.cpp:199-219).
    pub(crate) fn get_link(&self, is_first: bool) -> String {
        match self.get_type() {
            BackgroundFillType::Solid => get_color_hex_string(self.top_color),
            BackgroundFillType::Gradient => {
                let separator = if is_first { '?' } else { '&' };
                format!(
                    "{}-{}{}rotation={}",
                    get_color_hex_string(self.top_color),
                    get_color_hex_string(self.bottom_color),
                    separator,
                    self.rotation_angle
                )
            }
            BackgroundFillType::FreeformGradient => {
                let mut result = format!(
                    "{}~{}~{}",
                    get_color_hex_string(self.top_color),
                    get_color_hex_string(self.bottom_color),
                    get_color_hex_string(self.third_color)
                );
                if self.fourth_color != -1 {
                    result += "~";
                    result += &get_color_hex_string(self.fourth_color);
                }
                result
            }
        }
    }

    /// `BackgroundFill::is_dark` (BackgroundType.cpp:221-234).
    pub(crate) fn is_dark(&self) -> bool {
        match self.get_type() {
            BackgroundFillType::Solid => (self.top_color & 0x808080) == 0,
            BackgroundFillType::Gradient => {
                (self.top_color & 0x808080) == 0 && (self.bottom_color & 0x808080) == 0
            }
            BackgroundFillType::FreeformGradient => {
                (self.top_color & 0x808080) == 0
                    && (self.bottom_color & 0x808080) == 0
                    && (self.third_color & 0x808080) == 0
                    && (self.fourth_color == -1 || (self.fourth_color & 0x808080) == 0)
            }
        }
    }
}

impl BackgroundType {
    /// `BackgroundType(bool is_blurred, bool is_moving, dark_theme_dimming)`
    /// (BackgroundType.h:89-91).
    fn wallpaper(is_blurred: bool, is_moving: bool, dark_theme_dimming: i32) -> BackgroundType {
        BackgroundType {
            r#type: BackgroundTypeType::Wallpaper,
            is_blurred,
            is_moving,
            intensity: dark_theme_dimming,
            ..BackgroundType::default()
        }
    }

    /// `BackgroundType(bool is_moving, BackgroundFill &&, intensity)`
    /// (BackgroundType.h:92-94).
    fn pattern(is_moving: bool, fill: BackgroundFill, intensity: i32) -> BackgroundType {
        BackgroundType {
            r#type: BackgroundTypeType::Pattern,
            is_moving,
            intensity,
            fill,
            ..BackgroundType::default()
        }
    }

    /// `BackgroundType(BackgroundFill &&, dark_theme_dimming)`
    /// (BackgroundType.h:95-97).
    fn fill(fill: BackgroundFill, dark_theme_dimming: i32) -> BackgroundType {
        BackgroundType {
            r#type: BackgroundTypeType::Fill,
            intensity: dark_theme_dimming,
            fill,
            ..BackgroundType::default()
        }
    }

    /// `BackgroundType(string theme_name)` (BackgroundType.h:98-99).
    fn chat_theme(theme_name: String) -> BackgroundType {
        BackgroundType {
            r#type: BackgroundTypeType::ChatTheme,
            theme_name,
            ..BackgroundType::default()
        }
    }

    /// `BackgroundType::get_background_type` (BackgroundType.cpp:354-390).
    pub(crate) fn get_background_type(
        background_type: Option<&td_api::BackgroundType>,
        dark_theme_dimming: i32,
    ) -> std::result::Result<BackgroundType, Status> {
        let Some(background_type) = background_type else {
            return std::result::Result::Ok(BackgroundType::default());
        };
        if !(0..=100).contains(&dark_theme_dimming) {
            return std::result::Result::Err(Status::error(
                400,
                "Invalid dark them brightness specified",
            ));
        }

        match background_type {
            td_api::BackgroundType::backgroundTypeWallpaper(wallpaper_type) => {
                std::result::Result::Ok(BackgroundType::wallpaper(
                    wallpaper_type.is_blurred,
                    wallpaper_type.is_moving,
                    dark_theme_dimming,
                ))
            }
            td_api::BackgroundType::backgroundTypePattern(pattern_type) => {
                let background_fill =
                    BackgroundFill::get_background_fill(pattern_type.fill.as_ref())?;
                if !is_valid_intensity(pattern_type.intensity, false) {
                    return std::result::Result::Err(Status::error(400, "Wrong intensity value"));
                }
                let intensity = if pattern_type.is_inverted {
                    -std::cmp::max(pattern_type.intensity, 1)
                } else {
                    pattern_type.intensity
                };
                std::result::Result::Ok(BackgroundType::pattern(
                    pattern_type.is_moving,
                    background_fill,
                    intensity,
                ))
            }
            td_api::BackgroundType::backgroundTypeFill(fill_type) => {
                let background_fill = BackgroundFill::get_background_fill(fill_type.fill.as_ref())?;
                std::result::Result::Ok(BackgroundType::fill(background_fill, dark_theme_dimming))
            }
            td_api::BackgroundType::backgroundTypeChatTheme(chat_theme_type) => {
                std::result::Result::Ok(BackgroundType::chat_theme(
                    chat_theme_type.theme_name.clone(),
                ))
            }
        }
    }

    /// `BackgroundType::get_local_background_type` (BackgroundType.cpp:392-395).
    pub(crate) fn get_local_background_type(
        name: &str,
    ) -> std::result::Result<BackgroundType, Status> {
        let fill = BackgroundFill::get_background_fill_from_name(name)?;
        std::result::Result::Ok(BackgroundType::fill(fill, 0))
    }

    /// `BackgroundType::is_background_name_local` (BackgroundType.cpp:397-399).
    pub(crate) fn is_background_name_local(name: &str) -> bool {
        let query_pos = name.find('?');
        let name_bytes = name.as_bytes();
        name.len() <= 13
            || query_pos.is_some_and(|pos| pos <= 13)
            || !crate::misc::is_base64url_characters(
                &name_bytes[..query_pos.unwrap_or(name_bytes.len())],
            )
    }

    /// `BackgroundType::has_gradient_fill` (BackgroundType.h:117-119).
    #[allow(dead_code)] // the UploadBackgroundQuery seam's future net impl
    pub(crate) fn has_gradient_fill(&self) -> bool {
        self.r#type == BackgroundTypeType::Fill && self.fill.get_type() != BackgroundFillType::Solid
    }

    /// `BackgroundType::get_mime_type` (BackgroundType.cpp:247-250).
    pub(crate) fn get_mime_type(&self) -> &'static str {
        assert!(self.has_file());
        if self.r#type == BackgroundTypeType::Pattern {
            "image/png"
        } else {
            "image/jpeg"
        }
    }

    /// `BackgroundType::apply_parameters_from_link` (BackgroundType.cpp:252-286).
    pub(crate) fn apply_parameters_from_link(&mut self, name: &str) {
        let query = tdutils::http_url::parse_url_query(name);

        self.is_blurred = false;
        self.is_moving = false;
        if let Some(mode) = query.get_arg("mode") {
            for mode in mode.split(' ') {
                let mode = mode.to_lowercase();
                if self.r#type != BackgroundTypeType::Pattern && mode == "blur" {
                    self.is_blurred = true;
                }
                if mode == "motion" {
                    self.is_moving = true;
                }
            }
        }

        if self.r#type == BackgroundTypeType::Pattern {
            self.intensity = -101;
            if let Some(intensity_arg) = query.get_arg("intensity")
                && !intensity_arg.is_empty()
            {
                self.intensity = intensity_arg.parse::<i32>().unwrap_or(0);
            }
            if !is_valid_intensity(self.intensity, true) {
                self.intensity = 50;
            }

            if let Some(bg_color) = query.get_arg("bg_color")
                && !bg_color.is_empty()
            {
                let rotation = query.get_arg("rotation").unwrap_or("");
                let link = format!(
                    "{}?rotation={}",
                    tdutils::http_url::url_encode(bg_color),
                    tdutils::http_url::url_encode(rotation)
                );
                if let std::result::Result::Ok(fill) =
                    BackgroundFill::get_background_fill_from_name(&link)
                {
                    self.fill = fill;
                }
            }
        }
    }

    /// `BackgroundType::get_link` (BackgroundType.cpp:288-323).
    pub(crate) fn get_link(&self, is_first: bool) -> String {
        let mut mode = String::new();
        if self.is_blurred {
            mode = "blur".to_string();
        }
        if self.is_moving {
            if !mode.is_empty() {
                mode += "+";
            }
            mode += "motion";
        }

        match self.r#type {
            BackgroundTypeType::Wallpaper => {
                if !mode.is_empty() {
                    format!("mode={mode}")
                } else {
                    String::new()
                }
            }
            BackgroundTypeType::Pattern => {
                let mut link = format!(
                    "intensity={}&bg_color={}",
                    self.intensity,
                    self.fill.get_link(false)
                );
                if !mode.is_empty() {
                    link += "&mode=";
                    link += &mode;
                }
                link
            }
            BackgroundTypeType::Fill => self.fill.get_link(is_first),
            BackgroundTypeType::ChatTheme => String::new(),
        }
    }

    /// `BackgroundType::has_equal_type` (BackgroundType.h:127-129).
    pub(crate) fn has_equal_type(&self, other: &BackgroundType) -> bool {
        self.r#type == other.r#type
    }

    /// `BackgroundType::get_input_wallpaper_settings`
    /// (BackgroundType.cpp:483-510) — the mask bits follow the generated
    /// `WallPaperSettings` codec (blur 1, motion 2, intensity 3, second 4,
    /// third 5, fourth 6, emoticon 7; rotation shares the second bit).
    pub(crate) fn get_input_wallpaper_settings(&self) -> telegram_api::WallPaperSettings {
        let mut flags = 0;
        match self.fill.get_type() {
            BackgroundFillType::FreeformGradient => {
                if self.fill.fourth_color != -1 {
                    flags |= 1 << 6;
                }
                flags |= 1 << 5;
                flags |= 1 << 4;
                flags |= 1;
            }
            BackgroundFillType::Gradient => {
                flags |= 1 << 4;
                flags |= 1;
            }
            BackgroundFillType::Solid => {
                flags |= 1;
            }
        }
        if !self.theme_name.is_empty() {
            flags |= 1 << 7;
        }
        if self.intensity != 0 {
            flags |= 1 << 3;
        }
        telegram_api::WallPaperSettings {
            flags,
            blur: self.is_blurred,
            motion: self.is_moving,
            background_color: self.fill.top_color,
            second_background_color: self.fill.bottom_color,
            third_background_color: self.fill.third_color,
            fourth_background_color: self.fill.fourth_color,
            intensity: self.intensity,
            rotation: self.fill.rotation_angle,
            emoticon: self.theme_name.clone(),
        }
    }

    /// `BackgroundType::is_dark` (BackgroundType.h:135-138).
    pub(crate) fn is_dark(&self) -> bool {
        assert_eq!(self.r#type, BackgroundTypeType::Fill);
        self.fill.is_dark()
    }
}

// ==== the Background record + log events (BackgroundManager.cpp) ====

/// `BackgroundManager::Background` (BackgroundManager.h:52-92).
#[derive(Clone, Debug, PartialEq)]
pub struct Background {
    pub id: BackgroundId,
    pub access_hash: i64,
    pub name: String,
    pub file_id: FileId,
    pub is_creator: bool,
    pub is_default: bool,
    pub is_dark: bool,
    pub has_new_local_id: bool,
    pub type_: BackgroundType,
    pub file_source_id: FileSourceId,
}

impl Default for Background {
    /// The C++ member initializer (BackgroundManager.h:60):
    /// `has_new_local_id = true` — a fresh background gets a local id when
    /// its id is invalid (the no-file intake + the local creation paths).
    fn default() -> Background {
        Background {
            id: BackgroundId::new(0),
            access_hash: 0,
            name: String::new(),
            file_id: FileId::new(0, 0),
            is_creator: false,
            is_default: false,
            is_dark: false,
            has_new_local_id: true,
            type_: BackgroundType::default(),
            file_source_id: FileSourceId::new(0),
        }
    }
}

impl Background {
    /// `Background::store` (BackgroundManager.cpp:323-340) — byte-exact;
    /// the document row rides the env (`DocumentsManager::store_document`).
    pub fn store(&self, env: &mut impl BackgroundManagerEnv, out: &mut Vec<u8>) {
        let has_file_id = self.file_id.is_valid();
        store_parse::store_flags(
            out,
            [
                self.is_creator,
                self.is_default,
                self.is_dark,
                has_file_id,
                self.has_new_local_id,
            ],
        );
        store_parse::store_i64(out, self.id.get());
        store_parse::store_i64(out, self.access_hash);
        store_parse::store_string(out, &self.name);
        if has_file_id {
            env.store_document_row(self.file_id, out);
        }
        self.type_.store(out);
    }

    /// `Background::parse` (BackgroundManager.cpp:342-361) — byte-exact.
    pub fn parse(
        env: &mut impl BackgroundManagerEnv,
        parser: &mut Parser<'_>,
    ) -> std::result::Result<Background, &'static str> {
        let mut background = Background::default();
        let [
            is_creator,
            is_default,
            is_dark,
            has_file_id,
            has_new_local_id,
        ] = parser.parse_flags()?;
        background.is_creator = is_creator;
        background.is_default = is_default;
        background.is_dark = is_dark;
        background.has_new_local_id = has_new_local_id;
        background.id = BackgroundId::new(parser.i64_()?);
        background.access_hash = parser.i64_()?;
        background.name = parser.string()?;
        if has_file_id {
            background.file_id = env.parse_document_row(parser)?;
        }
        background.type_ = BackgroundType::parse(parser)?;
        std::result::Result::Ok(background)
    }
}

/// `BackgroundManager::BackgroundLogEvent` (BackgroundManager.cpp:363-379).
#[derive(Clone, Debug, Default)]
pub struct BackgroundLogEvent {
    pub background: Background,
    pub set_type: BackgroundType,
}

impl BackgroundLogEvent {
    pub fn store(&self, env: &mut impl BackgroundManagerEnv, out: &mut Vec<u8>) {
        self.background.store(env, out);
        self.set_type.store(out);
    }

    pub fn parse(
        env: &mut impl BackgroundManagerEnv,
        parser: &mut Parser<'_>,
    ) -> std::result::Result<BackgroundLogEvent, &'static str> {
        let background = Background::parse(env, parser)?;
        let set_type = BackgroundType::parse(parser)?;
        std::result::Result::Ok(BackgroundLogEvent {
            background,
            set_type,
        })
    }
}

/// `BackgroundManager::BackgroundsLogEvent` (BackgroundManager.cpp:381-394).
#[derive(Clone, Debug, Default)]
pub struct BackgroundsLogEvent {
    pub backgrounds: Vec<Background>,
}

impl BackgroundsLogEvent {
    pub fn store(&self, env: &mut impl BackgroundManagerEnv, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.backgrounds.len() as i32);
        for background in &self.backgrounds {
            background.store(env, out);
        }
    }

    pub fn parse(
        env: &mut impl BackgroundManagerEnv,
        parser: &mut Parser<'_>,
    ) -> std::result::Result<BackgroundsLogEvent, &'static str> {
        let count = parser.i32_()?;
        if count < 0 {
            return std::result::Result::Err("Invalid background count");
        }
        let mut backgrounds = Vec::with_capacity(count as usize);
        for _ in 0..count {
            backgrounds.push(Background::parse(env, parser)?);
        }
        std::result::Result::Ok(BackgroundsLogEvent { backgrounds })
    }
}

/// The `LocalBackgroundHash`/`LocalBackgroundEquals` key
/// (BackgroundManager.h:94-106) — the background identity used by the
/// local-background id cache: name, type and the three creator flags.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct LocalBackgroundKey {
    name: String,
    type_: BackgroundType,
    is_creator: bool,
    is_default: bool,
    is_dark: bool,
}

impl LocalBackgroundKey {
    fn of(background: &Background) -> LocalBackgroundKey {
        LocalBackgroundKey {
            name: background.name.clone(),
            type_: background.type_.clone(),
            is_creator: background.is_creator,
            is_default: background.is_default,
            is_dark: background.is_dark,
        }
    }
}

/// The `FileView` snapshot the env answers for the manager (the real
/// `FileView` borrows the file-manager lock).
#[derive(Clone, Copy, Debug, Default)]
pub struct FileViewInfo {
    pub is_encrypted: bool,
    pub has_full_local_location: bool,
    pub has_generate_location: bool,
    pub main_file_id: FileId,
}

/// The upload continuation the C++ models as the `set_dialog_background`
/// upload-promise lambda (`do_set_dialog_background` re-entry).
enum UploadedBackgroundTarget {
    /// `set_background`'s upload half (the `setDefaultBackground` promise).
    Plain {
        promise: Promise<Option<td_api::Background>>,
    },
    /// `set_dialog_background`'s upload half — on success,
    /// `do_set_dialog_background(dialog_id, uploaded_id, type_, for_both)`.
    SetDialog {
        dialog_id: DialogId,
        for_both: bool,
        promise: Promise<()>,
    },
}

/// `BackgroundManager::UploadedFileInfo` (BackgroundManager.h:161-173).
struct UploadedFileInfo {
    type_: BackgroundType,
    dialog_id: DialogId,
    for_dark_theme: bool,
    target: UploadedBackgroundTarget,
}

/// The BackgroundManager in-flight query kinds — the C++ ResultHandler
/// classes of the net-drain 33 families (BackgroundManager.cpp:46-304).
/// The handler state the answers read (the `background_id_`/name pair, the
/// dialog flags, the upload members, every `promise_`) rides the
/// `PendingBackgroundQueries` lots on the whole-Td env side — the kinds
/// stay `Copy`.
#[derive(Clone, Copy)]
pub(crate) enum BackgroundNetKind {
    /// `GetBackgroundQuery` (cpp:46) — `account.getWallPaper`; the answer
    /// runs the `on_get_background` intake and the SearchBackgroundRequest
    /// object conversion (the tries ladder rides the lot).
    GetBackground,
    /// `GetBackgroundsQuery` (cpp:81) — `account.getWallPapers`; the
    /// answer re-enters `on_get_backgrounds` (the manager's own pending
    /// queue carries the promises).
    GetBackgrounds,
    /// `SetChatWallPaperQuery` (cpp:107) — `messages.setChatWallPaper`
    /// (the `setChatBackground`/`deleteChatBackground` wire legs).
    SetChatWallPaper,
    /// `InstallBackgroundQuery` (cpp:171) — `account.installWallPaper`
    /// (the `setDefaultBackground` install tail).
    InstallBackground,
    /// `UploadBackgroundQuery` (cpp:198) — `account.uploadWallPaper` (the
    /// upload tail's wire half; the file-source delivery rides Files).
    UploadBackground,
    /// `UnsaveBackgroundQuery` (cpp:245) — `account.saveWallPaper` (the
    /// `removeInstalledBackground` wire leg).
    UnsaveBackground,
    /// `ResetBackgroundsQuery` (cpp:276) — `account.resetWallPapers` (the
    /// `resetInstalledBackgrounds` wire leg).
    ResetBackgrounds,
}

/// The [`BackgroundManager`] seams over the `Td` fields — the env
/// convention; the query seams dispatch over the `BackgroundNetKind`
/// plane (the whole-Td env overrides; the defaults keep the net-inert
/// posture) and the answer handlers are the manager methods.
#[allow(clippy::too_many_arguments)]
pub trait BackgroundManagerEnv {
    /// `td_->auth_manager_->is_bot()`.
    fn is_bot(&self) -> bool;

    /// `G()->close_status()`.
    fn close_status(&self) -> Status;

    /// `G()->td_db()->get_binlog_pmc()->get`.
    fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;

    /// `G()->td_db()->get_binlog_pmc()->set`.
    fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]);

    /// `G()->td_db()->get_binlog_pmc()->erase`.
    fn binlog_pmc_erase(&mut self, key: &str);

    /// `G()->use_sqlite_pmc()`.
    fn use_sqlite_pmc(&self) -> bool;

    /// `G()->td_db()->get_sqlite_pmc()->get` — the collapsed line inlines
    /// the async callback (the common kv plays the pmc role).
    fn sqlite_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;

    /// `G()->td_db()->get_sqlite_pmc()->set`.
    fn sqlite_pmc_set_raw(&mut self, key: &[u8], value: &[u8]);

    /// `send_closure(G()->td(), &Td::send_update, ...)`.
    fn send_update(&mut self, update: td_api::Update);

    /// `DocumentsManager::on_get_document(document, DialogId(), false,
    /// false, nullptr, Document::Type::General, subtype)` — `None` models
    /// the C++ empty/invalid document.
    fn on_get_document(
        &mut self,
        document: telegram_api::Document,
        document_subtype: crate::documents_manager::Subtype,
    ) -> Option<FileId>;

    /// `DocumentsManager::get_document_object(file_id, PhotoFormat::Png)`.
    fn get_document_object(
        &mut self,
        file_id: FileId,
        thumbnail_format: PhotoFormat,
    ) -> Option<td_api::Document>;

    /// `DocumentsManager::store_document(file_id, storer)`.
    fn store_document_row(&mut self, file_id: FileId, out: &mut Vec<u8>);

    /// `DocumentsManager::parse_document(parser)`.
    fn parse_document_row(
        &mut self,
        parser: &mut Parser<'_>,
    ) -> std::result::Result<FileId, &'static str>;

    /// `FileManager::get_input_file_id(FileType::Background, file, {},
    /// false, false)`.
    fn get_input_file_id(
        &mut self,
        type_: FileType,
        file: Option<&td_api::InputFile>,
    ) -> StatusResult<FileId>;

    /// `FileManager::get_file_view` — the snapshot over the file-manager
    /// lock.
    fn file_view(&mut self, file_id: FileId) -> Option<FileViewInfo>;

    /// `FileManager::merge(x_file_id, y_file_id)` — the caller logs the
    /// status.
    fn file_merge(&mut self, x: FileId, y: FileId) -> StatusResult<()>;

    /// `FileManager::cancel_upload(file_upload_id)`.
    fn file_cancel_upload(&mut self, file_upload_id: FileUploadId);

    /// `FileManager::add_file_source(file_id, source_id, "add_background")`.
    fn file_add_source(&mut self, file_id: FileId, source_id: FileSourceId, source: &str);

    /// `Document(Document::Type::General, file_id).get_file_ids(td_)` —
    /// the document's file ids (the main id + the thumbnail).
    fn document_file_ids(&mut self, file_id: FileId) -> Vec<FileId>;

    /// `FileReferenceManager::create_background_file_source(background_id,
    /// access_hash)` (FileReferenceManager.cpp:252-255).
    fn create_background_file_source(
        &mut self,
        background_id: BackgroundId,
        access_hash: i64,
    ) -> FileSourceId;

    /// `FileManager::get_internal_upload_id()` (FileManager.cpp).
    fn get_internal_upload_id(&mut self) -> i64;

    /// `DialogManager::check_dialog_access(dialog_id, allow_secret_chats,
    /// access_rights, source)` — the whole-Td env implements the real
    /// check; the theme env never answers dialog questions.
    fn check_dialog_access(
        &mut self,
        dialog_id: DialogId,
        _allow_secret_chats: bool,
        _access_rights: AccessRights,
        _source: &str,
    ) -> StatusResult<()> {
        let _ = dialog_id;
        tdutils::log_info!("DialogManager::check_dialog_access — [blocked: whole-Td env]");
        StatusResult::from_status(Status::error(400, "Chat not found"))
    }

    /// `ChatManager::get_channel_permissions(channel_id)`.
    fn get_channel_permissions(
        &mut self,
        _channel_id: ChannelId,
    ) -> Option<DialogParticipantStatus> {
        tdutils::log_info!("ChatManager::get_channel_permissions — [blocked: whole-Td env]");
        None
    }

    /// `UserManager::get_secret_chat_user_id(secret_chat_id)`.
    fn get_secret_chat_user_id(&mut self, _secret_chat_id: SecretChatId) -> UserId {
        tdutils::log_info!("UserManager::get_secret_chat_user_id — [blocked: whole-Td env]");
        UserId::new(0)
    }

    /// `FileManager::upload(file_upload_id, upload_background_file_callback_,
    /// 1, 0)` + the callback → `on_upload_background_file` /
    /// `on_upload_background_file_error` hop (BackgroundManager.cpp:306-317,
    /// 983-994). The collapsed line has no upload-completion pump yet (the
    /// M.12 transfer loop parks without the net) — the callback never fires;
    /// the answer handlers are real and unit-tested.
    fn start_background_file_upload(&mut self, file_upload_id: FileUploadId) {
        tdutils::log_info!(
            "Start background file upload {file_upload_id:?} — [blocked: upload-completion pump]"
        );
    }

    /// `FileManager::delete_partial_remote_location_if_needed`
    /// (FileManager.cpp) — the upload-query error tail's cleanup half; the
    /// whole-Td env runs the real call.
    fn file_delete_partial_remote_location_if_needed(
        &mut self,
        _file_upload_id: FileUploadId,
        _status: &Status,
    ) {
        tdutils::log_info!(
            "FileManager::delete_partial_remote_location_if_needed — [blocked: whole-Td env]"
        );
    }

    /// `GetBackgroundsQuery::send` (BackgroundManager.cpp:89-91) — the
    /// whole-Td env dispatches; the answer arrives through
    /// [`BackgroundManager::on_get_backgrounds`].
    fn send_get_backgrounds_query(&mut self) {
        tdutils::log_info!("Send account.getWallPapers — [net-inert]");
    }

    /// `GetBackgroundQuery::send` (BackgroundManager.cpp:55-60) — the
    /// whole-Td env dispatches; the answer arrives through
    /// [`BackgroundManager::on_get_background`] + the promise (the
    /// `SearchBackgroundRequest` re-entry state rides the parked lot:
    /// the full `search_name` whose link parameters the answer applies
    /// (cpp:548-550) and the `set_tries(3)` counter — 1 for the
    /// non-search reloads, the `load_user(id, tries)` precedent).
    fn send_get_background_query(
        &mut self,
        background_id: BackgroundId,
        name: &str,
        input_wallpaper: Option<telegram_api::InputWallPaper>,
        search_name: &str,
        tries_left: i32,
        promise: Promise<Option<td_api::Background>>,
    ) {
        let _ = (
            background_id,
            name,
            input_wallpaper,
            search_name,
            tries_left,
            promise,
        );
        tdutils::log_info!("Send account.getWallPaper — [net-inert]");
    }

    /// `InstallBackgroundQuery::send` (BackgroundManager.cpp:178-181) —
    /// the whole-Td env dispatches; the answer arrives through
    /// [`BackgroundManager::on_installed_background`]. The
    /// `background_id`/`type`/`for_dark_theme` members are the C++
    /// promise-lambda captures (cpp:899-903).
    fn send_install_background_query(
        &mut self,
        background_id: BackgroundId,
        input_wallpaper: Option<telegram_api::InputWallPaper>,
        type_: &BackgroundType,
        for_dark_theme: bool,
        promise: Promise<Option<td_api::Background>>,
    ) {
        let _ = (
            background_id,
            input_wallpaper,
            type_,
            for_dark_theme,
            promise,
        );
        tdutils::log_info!("Send account.installWallPaper — [net-inert]");
    }

    /// `UnsaveBackgroundQuery::send` (BackgroundManager.cpp:252-255) —
    /// the whole-Td env dispatches; the answer arrives through
    /// [`BackgroundManager::on_removed_background`].
    fn send_unsave_background_query(
        &mut self,
        background_id: BackgroundId,
        input_wallpaper: Option<telegram_api::InputWallPaper>,
        promise: Promise<()>,
    ) {
        let _ = (background_id, input_wallpaper, promise);
        tdutils::log_info!("Send account.saveWallPaper(unsave) — [net-inert]");
    }

    /// `ResetBackgroundsQuery::send` (BackgroundManager.cpp:283-285) —
    /// the whole-Td env dispatches; the answer arrives through
    /// [`BackgroundManager::on_reset_background`].
    fn send_reset_backgrounds_query(&mut self, promise: Promise<()>) {
        let _ = promise;
        tdutils::log_info!("Send account.resetWallPapers — [net-inert]");
    }

    /// `SetChatWallPaperQuery::send` (BackgroundManager.cpp:117-144) —
    /// the whole-Td env resolves the write-access input peer, computes the
    /// `is_remove_` flag (the `on_update_dialog_background` remove-half is
    /// the dialog-plane seam) and dispatches over the masks; the answer
    /// runs the `UpdatesManager::on_get_updates` result routing and the
    /// `WALLPAPER_NOT_FOUND` revert retry (cpp:146-168).
    fn send_set_chat_wallpaper_query(
        &mut self,
        dialog_id: DialogId,
        input_wallpaper: Option<telegram_api::InputWallPaper>,
        settings: Option<telegram_api::WallPaperSettings>,
        old_message_id: i32,
        for_both: bool,
        revert: bool,
        promise: Promise<()>,
    ) {
        let _ = (
            dialog_id,
            input_wallpaper,
            settings,
            old_message_id,
            for_both,
            revert,
            promise,
        );
        tdutils::log_info!("Send messages.setChatWallPaper — [net-inert]");
    }

    /// `UploadBackgroundQuery::send` (BackgroundManager.cpp:210-219) —
    /// the whole-Td env dispatches; the answer arrives through
    /// [`BackgroundManager::on_uploaded_background_file`] (the manager keeps
    /// the continuation in `pending_upload_queries` until then).
    fn send_upload_background_query(
        &mut self,
        file_upload_id: FileUploadId,
        input_file: Option<telegram_api::InputFile>,
        type_: &BackgroundType,
        dialog_id: DialogId,
        for_dark_theme: bool,
    ) {
        let _ = (file_upload_id, input_file, type_, dialog_id, for_dark_theme);
        tdutils::log_info!("Send account.uploadWallPaper — [net-inert]");
    }
}

/// The port of `td::BackgroundManager` — constructed at Td.cpp:1169-1171,
/// `start_up` at BackgroundManager.cpp:396.
#[derive(Default)]
pub struct BackgroundManager {
    /// The start_up-once guard (the C++ runs start_up once by construction).
    is_inited: bool,
    /// `backgrounds_`.
    backgrounds: HashMap<BackgroundId, Background>,
    /// `background_id_to_file_source_id_`.
    background_id_to_file_source_id: HashMap<BackgroundId, (i64, FileSourceId)>,
    /// `name_to_background_id_`.
    name_to_background_id: HashMap<String, BackgroundId>,
    /// `file_id_to_background_id_`.
    file_id_to_background_id: HashMap<FileId, BackgroundId>,
    /// `loaded_from_database_backgrounds_`.
    loaded_from_database_backgrounds: HashSet<String>,
    /// `set_background_id_[2]`.
    set_background_id: [BackgroundId; 2],
    /// `set_background_type_[2]`.
    set_background_type: [BackgroundType; 2],
    /// `installed_backgrounds_`.
    installed_backgrounds: Vec<(BackgroundId, BackgroundType)>,
    /// `pending_get_backgrounds_queries_`.
    pending_get_backgrounds_queries: Vec<(bool, Promise<td_api::Backgrounds>)>,
    /// `being_uploaded_files_`.
    being_uploaded_files: HashMap<FileUploadId, UploadedFileInfo>,
    /// The in-flight UploadBackgroundQuery continuations (the C++ carries
    /// the promise in the query handler — see the module doc).
    pending_upload_queries: HashMap<FileUploadId, UploadedFileInfo>,
    /// `local_backgrounds_`.
    local_backgrounds: HashMap<LocalBackgroundKey, BackgroundId>,
    /// `max_local_background_id_`.
    max_local_background_id: BackgroundId,
    /// `local_background_ids_[2]`.
    local_background_ids: [Vec<BackgroundId>; 2],
}

impl BackgroundManager {
    /// `BackgroundManager::BackgroundManager` (BackgroundManager.cpp:319-321).
    pub fn new() -> BackgroundManager {
        BackgroundManager::default()
    }

    /// `BackgroundManager::start_up` (BackgroundManager.cpp:396-474).
    pub fn init(&mut self, env: &mut impl BackgroundManagerEnv) {
        if env.is_bot() {
            return;
        }
        if self.is_inited {
            return;
        }
        self.is_inited = true;

        let max_bg_id = env.binlog_pmc_get_raw(b"max_bg_id");
        self.max_local_background_id = BackgroundId::new(
            std::str::from_utf8(&max_bg_id)
                .ok()
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(0),
        );

        // first parse all log events and fix max_local_background_id_ value
        let mut has_selected_background = [false; 2];
        let mut selected_background_log_event =
            [BackgroundLogEvent::default(), BackgroundLogEvent::default()];
        for for_dark_theme in [false, true] {
            let index = for_dark_theme as usize;
            let log_event_string = env
                .binlog_pmc_get_raw(Self::get_background_database_key(for_dark_theme).as_bytes());
            if !log_event_string.is_empty() {
                has_selected_background[index] = true;
                let mut parser = Parser::new_log_event(&log_event_string)
                    .expect("Failed to parse background log event version");
                match BackgroundLogEvent::parse(env, &mut parser) {
                    std::result::Result::Ok(event) => {
                        let background = &event.background;
                        if background.has_new_local_id
                            && background.id.is_local()
                            && !background.type_.has_file()
                            && background.id.get() > self.max_local_background_id.get()
                        {
                            self.set_max_local_background_id(env, background.id);
                        }
                        self.add_local_background_to_cache(background);
                        selected_background_log_event[index] = event;
                    }
                    std::result::Result::Err(status) => {
                        tdutils::log_error!(
                            "Failed to load selected background for dark theme {for_dark_theme}: {status}"
                        );
                        has_selected_background[index] = false;
                    }
                }
            }
        }

        for for_dark_theme in [false, true] {
            let index = for_dark_theme as usize;
            let log_event_string = env.binlog_pmc_get_raw(
                Self::get_local_backgrounds_database_key(for_dark_theme).as_bytes(),
            );
            if !log_event_string.is_empty() {
                let mut parser = Parser::new_log_event(&log_event_string)
                    .expect("Failed to parse backgrounds log event version");
                match BackgroundsLogEvent::parse(env, &mut parser) {
                    std::result::Result::Ok(event) => {
                        for background in event.backgrounds {
                            if !background.has_new_local_id
                                || !background.id.is_valid()
                                || !background.id.is_local()
                                || background.type_.has_file()
                                || background.file_id.is_valid()
                            {
                                // the C++ CHECKs
                                tdutils::log_error!(
                                    "Failed to load local background {} of {:?}",
                                    background.id.get(),
                                    background.type_
                                );
                                continue;
                            }
                            if background.id.get() > self.max_local_background_id.get() {
                                self.set_max_local_background_id(env, background.id);
                            }
                            self.add_local_background_to_cache(&background);
                            self.add_background(env, &background, true);
                            self.local_background_ids[index].push(background.id);
                        }
                    }
                    std::result::Result::Err(status) => {
                        tdutils::log_error!(
                            "Failed to load local backgrounds for dark theme {for_dark_theme}: {status}"
                        );
                    }
                }
            }
        }

        // then add selected backgrounds fixing their identifiers
        for for_dark_theme in [false, true] {
            let index = for_dark_theme as usize;
            if has_selected_background[index] {
                let mut background = selected_background_log_event[index].background.clone();
                let set_type = selected_background_log_event[index].set_type.clone();

                let mut need_resave = false;
                if !background.has_new_local_id && !background.type_.has_file() {
                    background.has_new_local_id = true;
                    self.set_local_background_id(env, &mut background);
                    need_resave = true;
                }

                if !background.id.is_valid() {
                    // the C++ CHECK
                    tdutils::log_error!("Failed to load selected background id");
                    continue;
                }
                if background.file_id.is_valid() != background.type_.has_file() {
                    tdutils::log_error!(
                        "Failed to load {} of {:?}",
                        background.id.get(),
                        background.type_
                    );
                    need_resave = true;
                } else {
                    self.set_background_id[index] = background.id;
                    self.set_background_type[index] = set_type;

                    self.add_background(env, &background, false);
                }

                if need_resave {
                    self.save_background_id(env, for_dark_theme);
                }
            }

            self.send_update_default_background(env, for_dark_theme);
        }
    }

    /// `BackgroundManager::get_backgrounds` (BackgroundManager.cpp:509-520).
    pub fn get_backgrounds(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        for_dark_theme: bool,
        promise: Promise<td_api::Backgrounds>,
    ) {
        self.pending_get_backgrounds_queries
            .push((for_dark_theme, promise));
        if self.pending_get_backgrounds_queries.len() == 1 {
            env.send_get_backgrounds_query();
        }
    }

    /// `BackgroundManager::on_get_backgrounds` (BackgroundManager.cpp:1368-1404)
    /// — the GetBackgroundsQuery answer.
    pub fn on_get_backgrounds(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        result: StatusResult<telegram_api::account_WallPapers>,
    ) {
        let promises = std::mem::take(&mut self.pending_get_backgrounds_queries);
        assert!(!promises.is_empty());

        let wallpapers = match result {
            StatusResult::Err(error) => {
                // do not clear installed_backgrounds_
                for (_, mut promise) in promises {
                    promise.set_error(error.clone());
                }
                return;
            }
            StatusResult::Ok(wallpapers) => wallpapers,
        };

        match wallpapers {
            telegram_api::account_WallPapers::account_wallPapersNotModified(_) => {
                for (for_dark_theme, mut promise) in promises {
                    promise.set_value(self.get_backgrounds_object(env, for_dark_theme));
                }
            }
            telegram_api::account_WallPapers::account_wallPapers(wallpapers) => {
                self.installed_backgrounds.clear();
                for wallpaper in wallpapers.wallpapers {
                    if let Some(background) = self.on_get_background(
                        env,
                        BackgroundId::new(0),
                        "",
                        Some(wallpaper),
                        false,
                        false,
                        false,
                    ) {
                        self.installed_backgrounds.push(background);
                    }
                }
                for (for_dark_theme, mut promise) in promises {
                    promise.set_value(self.get_backgrounds_object(env, for_dark_theme));
                }
            }
        }
    }

    /// `BackgroundManager::reload_background` (BackgroundManager.cpp:531-535)
    /// — the FileSourceBackground repair entry (FileReferenceManager.cpp:533).
    pub fn reload_background(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        background_id: BackgroundId,
        access_hash: i64,
        promise: Promise<Option<td_api::Background>>,
    ) {
        self.reload_background_from_server(
            env,
            background_id,
            "",
            Some(telegram_api::InputWallPaper::inputWallPaper(
                telegram_api::inputWallPaper {
                    id: background_id.get(),
                    access_hash,
                },
            )),
            "",
            1,
            promise,
        );
    }

    /// `BackgroundManager::reload_background_from_server`
    /// (BackgroundManager.cpp:522-529) — the re-entry state params are the
    /// collapsed `SearchBackgroundRequest` members ("" / 1 for the
    /// non-search reloads).
    #[allow(clippy::too_many_arguments)]
    fn reload_background_from_server(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        background_id: BackgroundId,
        background_name: &str,
        input_wallpaper: Option<telegram_api::InputWallPaper>,
        search_name: &str,
        tries_left: i32,
        mut promise: Promise<Option<td_api::Background>>,
    ) {
        if let status = env.close_status()
            && status.is_error()
        {
            promise.set_error(status);
            return;
        }
        env.send_get_background_query(
            background_id,
            background_name,
            input_wallpaper,
            search_name,
            tries_left,
            promise,
        );
    }

    /// `BackgroundManager::search_background` (BackgroundManager.cpp:537-586).
    pub fn search_background(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        name: &str,
        mut promise: Promise<Option<td_api::Background>>,
    ) {
        let params_pos = name.find('?').unwrap_or(name.len());
        let slug = &name[..params_pos];
        if let Some(&background_id) = self.name_to_background_id.get(slug) {
            assert!(!BackgroundType::is_background_name_local(slug));

            let mut type_ = match self.backgrounds.get(&background_id) {
                Some(background) => background.type_.clone(),
                None => {
                    // the C++ CHECK
                    promise.set_value(None);
                    return;
                }
            };
            type_.apply_parameters_from_link(name);
            promise.set_value(self.get_background_object(env, background_id, false, Some(&type_)));
            return;
        }

        if slug.is_empty() {
            promise.set_error(Status::error(400, "Background name must be non-empty"));
            return;
        }

        if BackgroundType::is_background_name_local(slug) {
            match BackgroundType::get_local_background_type(name) {
                std::result::Result::Ok(type_) => {
                    let background_id = self.add_local_background(env, type_.clone());
                    promise.set_value(self.get_background_object(
                        env,
                        background_id,
                        false,
                        Some(&type_),
                    ));
                }
                std::result::Result::Err(status) => {
                    promise.set_error(status);
                }
            }
            return;
        }

        if env.use_sqlite_pmc() && !self.loaded_from_database_backgrounds.contains(slug) {
            tdutils::log_info!("Trying to load background {slug} from database");
            let value = env.sqlite_pmc_get_raw(&Self::get_background_name_database_key(slug));
            self.on_load_background_from_database(env, slug, value, promise);
            return;
        }

        self.reload_background_from_server(
            env,
            BackgroundId::new(0),
            slug,
            Some(telegram_api::InputWallPaper::inputWallPaperSlug(
                telegram_api::inputWallPaperSlug {
                    slug: slug.to_string(),
                },
            )),
            name,
            3,
            promise,
        );
    }

    /// `BackgroundManager::on_load_background_from_database`
    /// (BackgroundManager.cpp:588-620) — the collapsed line inlines the
    /// async sqlite callback; the promise is the search promise itself
    /// (the C++ `set_promises` resolves the actor's Unit promise with the
    /// captured empty pair, so the C++ answer is the 404 — kept).
    fn on_load_background_from_database(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        name: &str,
        value: Vec<u8>,
        mut promise: Promise<Option<td_api::Background>>,
    ) {
        if env.close_status().is_error() {
            return;
        }

        self.loaded_from_database_backgrounds
            .insert(name.to_string());

        assert!(!BackgroundType::is_background_name_local(name));
        if !self.name_to_background_id.contains_key(name) && !value.is_empty() {
            tdutils::log_info!(
                "Successfully loaded background {name} of size {} from database",
                value.len()
            );
            match Parser::new_log_event(&value) {
                Ok(mut parser) => match Background::parse(env, &mut parser) {
                    std::result::Result::Ok(background)
                        if background.type_.has_file()
                            && background.file_id.is_valid()
                            && background.id.is_valid() =>
                    {
                        if background.name != name {
                            tdutils::log_error!(
                                "Expected background {name}, but received {}",
                                background.name
                            );
                            self.name_to_background_id
                                .insert(name.to_string(), background.id);
                        }
                        self.add_local_background_to_cache(&background);
                        self.add_background(env, &background, false);
                    }
                    std::result::Result::Ok(background) => {
                        tdutils::log_error!(
                            "Can't load background {name}: the background {} is invalid",
                            background.id.get()
                        );
                    }
                    std::result::Result::Err(status) => {
                        tdutils::log_error!("Can't load background {name}: {status}");
                    }
                },
                Err(status) => {
                    tdutils::log_error!("Can't load background {name}: {status}");
                }
            }
        }

        // the C++ first-search 404: the request actor answers with the
        // empty pair captured at do_run
        promise.set_value(None);
    }

    /// `BackgroundManager::get_update_default_background_object`
    /// (BackgroundManager.cpp:622-627).
    fn get_update_default_background_object(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        for_dark_theme: bool,
    ) -> td_api::Update {
        let index = for_dark_theme as usize;
        let background_id = self.set_background_id[index];
        let background_type = self.set_background_type[index].clone();
        let background =
            self.get_background_object(env, background_id, for_dark_theme, Some(&background_type));
        td_api::Update::updateDefaultBackground(td_api::updateDefaultBackground {
            for_dark_theme,
            background,
        })
    }

    /// `BackgroundManager::send_update_default_background`
    /// (BackgroundManager.cpp:629-631).
    fn send_update_default_background(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        for_dark_theme: bool,
    ) {
        let update = self.get_update_default_background_object(env, for_dark_theme);
        env.send_update(update);
    }

    /// `BackgroundManager::prepare_input_file` (BackgroundManager.cpp:633-644).
    fn prepare_input_file(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        input_file: Option<&td_api::InputFile>,
    ) -> StatusResult<FileId> {
        let file_id = match env.get_input_file_id(FileType::Background, input_file) {
            StatusResult::Ok(file_id) => file_id,
            StatusResult::Err(status) => return StatusResult::from_status(status),
        };

        let Some(file_view) = env.file_view(file_id) else {
            return StatusResult::from_status(Status::error(400, "File not found"));
        };
        if file_view.is_encrypted {
            return StatusResult::from_status(Status::error(400, "Can't use encrypted file"));
        }
        if !file_view.has_full_local_location && !file_view.has_generate_location {
            return StatusResult::from_status(Status::error(
                400,
                "Need local or generate location to upload background",
            ));
        }
        StatusResult::Ok(file_id)
    }

    /// `BackgroundManager::set_max_local_background_id`
    /// (BackgroundManager.cpp:646-651).
    fn set_max_local_background_id(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        background_id: BackgroundId,
    ) {
        assert!(background_id.is_local());
        assert!(background_id.get() > self.max_local_background_id.get());
        self.max_local_background_id = background_id;
        env.binlog_pmc_set_raw(b"max_bg_id", background_id.get().to_string().as_bytes());
    }

    /// `BackgroundManager::get_next_local_background_id`
    /// (BackgroundManager.cpp:653-656).
    fn get_next_local_background_id(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
    ) -> BackgroundId {
        self.set_max_local_background_id(
            env,
            BackgroundId::new(self.max_local_background_id.get() + 1),
        );
        self.max_local_background_id
    }

    /// `BackgroundManager::set_local_background_id`
    /// (BackgroundManager.cpp:658-666).
    fn set_local_background_id(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        background: &mut Background,
    ) {
        assert!(!background.name.is_empty() || background.type_ != BackgroundType::default());
        assert!(background.has_new_local_id);
        let key = LocalBackgroundKey::of(background);
        let background_id = match self.local_backgrounds.get(&key) {
            Some(&background_id) => background_id,
            None => {
                let background_id = self.get_next_local_background_id(env);
                self.local_backgrounds.insert(key, background_id);
                background_id
            }
        };
        background.id = background_id;
    }

    /// `BackgroundManager::add_local_background_to_cache`
    /// (BackgroundManager.cpp:668-676).
    fn add_local_background_to_cache(&self, background: &Background) {
        if !background.has_new_local_id || !background.id.is_local() {
            return;
        }
        let _ = self
            .local_backgrounds
            .get(&LocalBackgroundKey::of(background));
        // the C++ emplace keeps the existing id; the entry is only used
        // through set_local_background_id, which allocates on absence
    }

    /// `BackgroundManager::add_local_background` (BackgroundManager.cpp:678-689).
    fn add_local_background(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        type_: BackgroundType,
    ) -> BackgroundId {
        let mut background = Background {
            is_creator: true,
            is_dark: type_.is_dark(),
            type_,
            ..Background::default()
        };
        background.name = background.type_.get_link(true);
        self.set_local_background_id(env, &mut background);
        let background_id = background.id;
        self.add_background(env, &background, true);
        background_id
    }

    /// `BackgroundManager::set_background` (BackgroundManager.cpp:691-743)
    /// — the `setDefaultBackground` surface.
    pub fn set_background(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        input_background: Option<&td_api::InputBackground>,
        background_type: Option<&td_api::BackgroundType>,
        for_dark_theme: bool,
        mut promise: Promise<Option<td_api::Background>>,
    ) {
        let type_ = match BackgroundType::get_background_type(background_type, 0) {
            std::result::Result::Ok(type_) => type_,
            std::result::Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };

        let Some(input_background) = input_background else {
            if type_.has_file() || background_type.is_none() {
                promise.set_error(Status::error(
                    400,
                    "Input background must be non-empty for the background type",
                ));
                return;
            }
            if matches!(
                background_type,
                Some(td_api::BackgroundType::backgroundTypeChatTheme(_))
            ) {
                promise.set_error(Status::error(400, "Background type isn't supported"));
                return;
            }

            let background_id = self.add_local_background(env, type_.clone());
            self.set_background_id(env, background_id, type_.clone(), for_dark_theme);

            self.local_background_ids[for_dark_theme as usize].insert(0, background_id);
            self.save_local_backgrounds(env, for_dark_theme);

            promise.set_value(self.get_background_object(env, background_id, for_dark_theme, None));
            return;
        };

        match input_background {
            td_api::InputBackground::inputBackgroundLocal(background_local) => {
                if !type_.has_file() {
                    promise.set_error(Status::error(
                        400,
                        "Can't specify local file for the background type",
                    ));
                    return;
                }
                assert!(background_type.is_some());

                let file_id =
                    match self.prepare_input_file(env, background_local.background.as_ref()) {
                        StatusResult::Ok(file_id) => file_id,
                        StatusResult::Err(status) => {
                            promise.set_error(status);
                            return;
                        }
                    };
                tdutils::log_info!("Receive file {file_id:?} for input background");
                assert!(file_id.is_valid());

                if let Some(&background_id) = self.file_id_to_background_id.get(&file_id) {
                    self.install_background(env, background_id, type_, for_dark_theme, promise);
                    return;
                }

                self.upload_background_file(
                    env,
                    file_id,
                    type_,
                    DialogId::new(0),
                    for_dark_theme,
                    UploadedBackgroundTarget::Plain { promise },
                );
            }
            td_api::InputBackground::inputBackgroundRemote(background_remote) => {
                self.install_background(
                    env,
                    BackgroundId::new(background_remote.background_id),
                    type_,
                    for_dark_theme,
                    promise,
                );
            }
            td_api::InputBackground::inputBackgroundPrevious(_) => {
                promise.set_error(Status::error(400, "Can't use a previous background"));
            }
        }
    }

    /// `BackgroundManager::delete_background` (BackgroundManager.cpp:745-748)
    /// — the `deleteDefaultBackground` surface.
    pub fn delete_background(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        for_dark_theme: bool,
        mut promise: Promise<()>,
    ) {
        self.set_background_id(
            env,
            BackgroundId::new(0),
            BackgroundType::default(),
            for_dark_theme,
        );
        promise.set_value(());
    }

    /// `BackgroundManager::get_background_dialog`
    /// (BackgroundManager.cpp:750-777).
    fn get_background_dialog(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        dialog_id: DialogId,
    ) -> StatusResult<DialogId> {
        if let StatusResult::Err(status) = env.check_dialog_access(
            dialog_id,
            true,
            AccessRights::Write,
            "get_background_dialog",
        ) {
            return StatusResult::from_status(status);
        }

        match dialog_id.get_type() {
            DialogType::User => StatusResult::Ok(dialog_id),
            DialogType::Chat => {
                StatusResult::from_status(Status::error(400, "Can't change background in the chat"))
            }
            DialogType::Channel => {
                let channel_id = dialog_id.get_channel_id();
                let has_rights =
                    env.get_channel_permissions(channel_id)
                        .is_some_and(|permissions| {
                            permissions.can_change_info_and_settings_as_administrator()
                        });
                if !has_rights {
                    return StatusResult::from_status(Status::error(
                        400,
                        "Not enough rights in the chat",
                    ));
                }
                StatusResult::Ok(dialog_id)
            }
            DialogType::SecretChat => {
                let user_id = env.get_secret_chat_user_id(dialog_id.get_secret_chat_id());
                if !user_id.is_valid() {
                    return StatusResult::from_status(Status::error(400, "Can't access the user"));
                }
                // `DialogId(user_id)` (BackgroundManager.cpp:770)
                StatusResult::Ok(DialogId::from_user_id(user_id))
            }
            DialogType::None => {
                unreachable!();
            }
        }
    }

    /// `BackgroundManager::set_dialog_background`
    /// (BackgroundManager.cpp:779-842) — the `setChatBackground` surface.
    #[allow(clippy::too_many_arguments)]
    pub fn set_dialog_background(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        dialog_id: DialogId,
        input_background: Option<&td_api::InputBackground>,
        background_type: Option<&td_api::BackgroundType>,
        dark_theme_dimming: i32,
        for_both: bool,
        mut promise: Promise<()>,
    ) {
        let dialog_id = match self.get_background_dialog(env, dialog_id) {
            StatusResult::Ok(dialog_id) => dialog_id,
            StatusResult::Err(status) => {
                promise.set_error(status);
                return;
            }
        };

        let type_ = match BackgroundType::get_background_type(background_type, dark_theme_dimming) {
            std::result::Result::Ok(type_) => type_,
            std::result::Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };

        let Some(input_background) = input_background else {
            if type_.has_file() || background_type.is_none() {
                promise.set_error(Status::error(
                    400,
                    "Input background must be non-empty for the background type",
                ));
                return;
            }
            self.send_set_dialog_background_query(
                env,
                dialog_id,
                Some(telegram_api::InputWallPaper::inputWallPaperNoFile(
                    telegram_api::inputWallPaperNoFile { id: 0 },
                )),
                Some(type_.get_input_wallpaper_settings()),
                0,
                for_both,
                promise,
            );
            return;
        };

        match input_background {
            td_api::InputBackground::inputBackgroundLocal(background_local) => {
                if !type_.has_file() {
                    promise.set_error(Status::error(
                        400,
                        "Can't specify local file for the background type",
                    ));
                    return;
                }
                assert!(background_type.is_some());

                let file_id =
                    match self.prepare_input_file(env, background_local.background.as_ref()) {
                        StatusResult::Ok(file_id) => file_id,
                        StatusResult::Err(status) => {
                            promise.set_error(status);
                            return;
                        }
                    };
                tdutils::log_info!("Receive file {file_id:?} for input background");
                assert!(file_id.is_valid());

                if let Some(&background_id) = self.file_id_to_background_id.get(&file_id) {
                    self.do_set_dialog_background(
                        env,
                        dialog_id,
                        background_id,
                        type_,
                        for_both,
                        promise,
                    );
                    return;
                }

                self.upload_background_file(
                    env,
                    file_id,
                    type_,
                    dialog_id,
                    false,
                    UploadedBackgroundTarget::SetDialog {
                        dialog_id,
                        for_both,
                        promise,
                    },
                );
            }
            td_api::InputBackground::inputBackgroundRemote(background_remote) => {
                self.do_set_dialog_background(
                    env,
                    dialog_id,
                    BackgroundId::new(background_remote.background_id),
                    type_,
                    for_both,
                    promise,
                );
            }
            td_api::InputBackground::inputBackgroundPrevious(background_previous) => {
                let message_id = MessageId::new(background_previous.message_id);
                if !message_id.is_server() {
                    promise.set_error(Status::error(400, "Invalid message identifier specified"));
                    return;
                }
                let settings = if background_type.is_none() {
                    None
                } else {
                    Some(type_.get_input_wallpaper_settings())
                };
                self.send_set_dialog_background_query(
                    env,
                    dialog_id,
                    None,
                    settings,
                    message_id.get_server_message_id().get(),
                    for_both,
                    promise,
                );
            }
        }
    }

    /// `BackgroundManager::delete_dialog_background`
    /// (BackgroundManager.cpp:844-848) — the `deleteChatBackground` surface.
    pub fn delete_dialog_background(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        dialog_id: DialogId,
        restore_previous: bool,
        mut promise: Promise<()>,
    ) {
        let dialog_id = match self.get_background_dialog(env, dialog_id) {
            StatusResult::Ok(dialog_id) => dialog_id,
            StatusResult::Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        env.send_set_chat_wallpaper_query(
            dialog_id,
            None,
            None,
            0,
            false,
            restore_previous,
            promise,
        );
    }

    /// `BackgroundManager::do_set_dialog_background`
    /// (BackgroundManager.cpp:850-866).
    #[allow(clippy::too_many_arguments)]
    fn do_set_dialog_background(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        dialog_id: DialogId,
        background_id: BackgroundId,
        type_: BackgroundType,
        for_both: bool,
        mut promise: Promise<()>,
    ) {
        if let status = env.close_status()
            && status.is_error()
        {
            promise.set_error(status);
            return;
        }
        let (access_hash, type_) = match self.backgrounds.get(&background_id) {
            Some(background) => {
                let type_ = if !type_.has_file() {
                    background.type_.clone()
                } else if !background.type_.has_equal_type(&type_) {
                    promise.set_error(Status::error(400, "Background type mismatch"));
                    return;
                } else {
                    type_
                };
                (background.access_hash, type_)
            }
            None => {
                promise.set_error(Status::error(400, "Background to set not found"));
                return;
            }
        };

        self.send_set_dialog_background_query(
            env,
            dialog_id,
            Some(telegram_api::InputWallPaper::inputWallPaper(
                telegram_api::inputWallPaper {
                    id: background_id.get(),
                    access_hash,
                },
            )),
            Some(type_.get_input_wallpaper_settings()),
            0,
            for_both,
            promise,
        );
    }

    /// `BackgroundManager::send_set_dialog_background_query`
    /// (BackgroundManager.cpp:868-874).
    #[allow(clippy::too_many_arguments)]
    fn send_set_dialog_background_query(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        dialog_id: DialogId,
        input_wallpaper: Option<telegram_api::InputWallPaper>,
        settings: Option<telegram_api::WallPaperSettings>,
        old_message_id: i32,
        for_both: bool,
        promise: Promise<()>,
    ) {
        env.send_set_chat_wallpaper_query(
            dialog_id,
            input_wallpaper,
            settings,
            old_message_id,
            for_both,
            false,
            promise,
        );
    }

    /// `BackgroundManager::set_background(BackgroundId, BackgroundType, ...)`
    /// (BackgroundManager.cpp:876-907) — the install path shared by the
    /// remote-input half and the reupload half.
    fn install_background(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        background_id: BackgroundId,
        type_: BackgroundType,
        for_dark_theme: bool,
        mut promise: Promise<Option<td_api::Background>>,
    ) {
        tdutils::log_info!("Set {background_id:?} with {type_:?}");
        let (access_hash, type_) = match self.backgrounds.get(&background_id) {
            Some(background) => {
                let type_ = if !type_.has_file() {
                    background.type_.clone()
                } else if !background.type_.has_equal_type(&type_) {
                    promise.set_error(Status::error(400, "Background type mismatch"));
                    return;
                } else {
                    type_
                };
                (background.access_hash, type_)
            }
            None => {
                promise.set_error(Status::error(400, "Background to set not found"));
                return;
            }
        };
        let index = for_dark_theme as usize;
        if self.set_background_id[index] == background_id
            && self.set_background_type[index] == type_
        {
            promise.set_value(self.get_background_object(env, background_id, for_dark_theme, None));
            return;
        }

        tdutils::log_info!("Install {background_id:?} with {type_:?}");

        if !type_.has_file() {
            self.set_background_id(env, background_id, type_, for_dark_theme);
            promise.set_value(self.get_background_object(env, background_id, for_dark_theme, None));
            return;
        }

        env.send_install_background_query(
            background_id,
            Some(telegram_api::InputWallPaper::inputWallPaper(
                telegram_api::inputWallPaper {
                    id: background_id.get(),
                    access_hash,
                },
            )),
            &type_,
            for_dark_theme,
            promise,
        );
    }

    /// `BackgroundManager::on_installed_background`
    /// (BackgroundManager.cpp:909-928) — the InstallBackgroundQuery answer.
    pub fn on_installed_background(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        background_id: BackgroundId,
        type_: BackgroundType,
        for_dark_theme: bool,
        result: StatusResult<()>,
        mut promise: Promise<Option<td_api::Background>>,
    ) {
        match result {
            StatusResult::Err(error) => {
                promise.set_error(error);
                return;
            }
            StatusResult::Ok(()) => {}
        }

        if let Some(installed) = self
            .installed_backgrounds
            .iter_mut()
            .find(|installed| installed.0 == background_id)
        {
            installed.1 = type_.clone();
        } else {
            self.installed_backgrounds
                .insert(0, (background_id, type_.clone()));
        }
        // the installed borrow ends before the state setter takes &mut self
        self.set_background_id(env, background_id, type_, for_dark_theme);
        promise.set_value(self.get_background_object(env, background_id, for_dark_theme, None));
    }

    /// `BackgroundManager::get_background_database_key`
    /// (BackgroundManager.cpp:930-932).
    fn get_background_database_key(for_dark_theme: bool) -> &'static str {
        if for_dark_theme { "bgd" } else { "bg" }
    }

    /// `BackgroundManager::get_local_backgrounds_database_key`
    /// (BackgroundManager.cpp:934-936).
    fn get_local_backgrounds_database_key(for_dark_theme: bool) -> &'static str {
        if for_dark_theme { "bgsd" } else { "bgs" }
    }

    /// `BackgroundManager::save_background_id`
    /// (BackgroundManager.cpp:938-949).
    fn save_background_id(&mut self, env: &mut impl BackgroundManagerEnv, for_dark_theme: bool) {
        let key = Self::get_background_database_key(for_dark_theme);
        let index = for_dark_theme as usize;
        let background_id = self.set_background_id[index];
        if background_id.is_valid() {
            let Some(background) = self.backgrounds.get(&background_id) else {
                // the C++ CHECK
                return;
            };
            let log_event = BackgroundLogEvent {
                background: background.clone(),
                set_type: self.set_background_type[index].clone(),
            };
            let mut out = Vec::new();
            store_parse::store_log_event_version(&mut out);
            log_event.store(env, &mut out);
            env.binlog_pmc_set_raw(key.as_bytes(), &out);
        } else {
            env.binlog_pmc_erase(key);
        }
    }

    /// `BackgroundManager::set_background_id`
    /// (BackgroundManager.cpp:951-961).
    fn set_background_id(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        background_id: BackgroundId,
        type_: BackgroundType,
        for_dark_theme: bool,
    ) {
        let index = for_dark_theme as usize;
        if background_id == self.set_background_id[index]
            && self.set_background_type[index] == type_
        {
            return;
        }

        self.set_background_id[index] = background_id;
        self.set_background_type[index] = type_;

        self.save_background_id(env, for_dark_theme);
        self.send_update_default_background(env, for_dark_theme);
    }

    /// `BackgroundManager::save_local_backgrounds`
    /// (BackgroundManager.cpp:963-981).
    fn save_local_backgrounds(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        for_dark_theme: bool,
    ) {
        let key = Self::get_local_backgrounds_database_key(for_dark_theme);
        let index = for_dark_theme as usize;
        const MAX_LOCAL_BACKGROUNDS: usize = 100;
        while self.local_background_ids[index].len() > MAX_LOCAL_BACKGROUNDS {
            self.local_background_ids[index].pop();
        }
        if !self.local_background_ids[index].is_empty() {
            let mut log_event = BackgroundsLogEvent::default();
            for background_id in &self.local_background_ids[index] {
                let Some(background) = self.backgrounds.get(background_id) else {
                    // the C++ CHECK
                    return;
                };
                log_event.backgrounds.push(background.clone());
            }
            let mut out = Vec::new();
            store_parse::store_log_event_version(&mut out);
            log_event.store(env, &mut out);
            env.binlog_pmc_set_raw(key.as_bytes(), &out);
        } else {
            env.binlog_pmc_erase(key);
        }
    }

    /// `BackgroundManager::upload_background_file`
    /// (BackgroundManager.cpp:983-994).
    fn upload_background_file(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        file_id: FileId,
        type_: BackgroundType,
        dialog_id: DialogId,
        for_dark_theme: bool,
        target: UploadedBackgroundTarget,
    ) {
        let file_upload_id = FileUploadId::new(file_id, env.get_internal_upload_id());
        assert!(file_id.is_valid());
        let is_inserted = self
            .being_uploaded_files
            .insert(
                file_upload_id,
                UploadedFileInfo {
                    type_,
                    dialog_id,
                    for_dark_theme,
                    target,
                },
            )
            .is_none();
        assert!(is_inserted);
        tdutils::log_info!("Ask to upload background {file_upload_id:?}");
        env.start_background_file_upload(file_upload_id);
    }

    /// `BackgroundManager::on_upload_background_file`
    /// (BackgroundManager.cpp:996-1015) — the UploadBackgroundFileCallback
    /// ok half; the future upload pump delivers the resolved `InputFile`
    /// (the Rust `UploadCallback` collapse carries only the id).
    pub fn on_upload_background_file(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        file_upload_id: FileUploadId,
        input_file: Option<telegram_api::InputFile>,
    ) {
        if env.close_status().is_error() {
            return;
        }

        tdutils::log_info!("Background {file_upload_id:?} has been uploaded");

        let Some(info) = self.being_uploaded_files.remove(&file_upload_id) else {
            // the C++ CHECK
            return;
        };

        self.do_upload_background_file(
            env,
            file_upload_id,
            info.type_,
            info.dialog_id,
            info.for_dark_theme,
            input_file,
            info.target,
        );
    }

    /// `BackgroundManager::on_upload_background_file_error`
    /// (BackgroundManager.cpp:1017-1034) — the UploadBackgroundFileCallback
    /// error half.
    pub fn on_upload_background_file_error(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        file_upload_id: FileUploadId,
        status: Status,
    ) {
        if env.close_status().is_error() {
            return;
        }

        tdutils::log_warning!("Background {file_upload_id:?} has upload error {status}");
        assert!(status.is_error());

        let Some(info) = self.being_uploaded_files.remove(&file_upload_id) else {
            // the C++ CHECK
            return;
        };

        // TODO CHECK that status has always a code
        let code = if status.code() > 0 {
            status.code()
        } else {
            500
        };
        let error = Status::error(code, status.message());
        match info.target {
            UploadedBackgroundTarget::Plain { mut promise } => {
                promise.set_error(error);
            }
            UploadedBackgroundTarget::SetDialog { mut promise, .. } => {
                promise.set_error(error);
            }
        }
    }

    /// `BackgroundManager::do_upload_background_file`
    /// (BackgroundManager.cpp:1036-1056).
    #[allow(clippy::too_many_arguments)]
    fn do_upload_background_file(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        file_upload_id: FileUploadId,
        type_: BackgroundType,
        dialog_id: DialogId,
        for_dark_theme: bool,
        input_file: Option<telegram_api::InputFile>,
        target: UploadedBackgroundTarget,
    ) {
        if let status = env.close_status()
            && status.is_error()
        {
            fail_upload_target(target, &status);
            return;
        }

        let Some(input_file) = input_file else {
            let Some(file_view) = env.file_view(file_upload_id.file_id) else {
                fail_upload_target(target, &Status::error(500, "Failed to reupload background"));
                return;
            };
            let main_file_id = file_view.main_file_id;
            if let Some(&background_id) = self.file_id_to_background_id.get(&main_file_id) {
                if dialog_id.is_valid() {
                    // the C++ set_background tail: promise.set_value(
                    // get_background_object(...)) → the lambda →
                    // do_set_dialog_background
                    match target {
                        UploadedBackgroundTarget::Plain { mut promise } => {
                            promise.set_value(self.get_background_object(
                                env,
                                background_id,
                                for_dark_theme,
                                None,
                            ));
                        }
                        UploadedBackgroundTarget::SetDialog {
                            dialog_id,
                            for_both,
                            promise,
                        } => {
                            self.do_set_dialog_background(
                                env,
                                dialog_id,
                                background_id,
                                type_,
                                for_both,
                                promise,
                            );
                        }
                    }
                    return;
                }
                match target {
                    UploadedBackgroundTarget::Plain { promise } => {
                        self.install_background(env, background_id, type_, for_dark_theme, promise);
                    }
                    UploadedBackgroundTarget::SetDialog { mut promise, .. } => {
                        promise.set_error(Status::error(500, "Failed to reupload background"));
                    }
                }
                return;
            }
            fail_upload_target(target, &Status::error(500, "Failed to reupload background"));
            return;
        };

        env.send_upload_background_query(
            file_upload_id,
            Some(input_file),
            &type_,
            dialog_id,
            for_dark_theme,
        );
        self.pending_upload_queries.insert(
            file_upload_id,
            UploadedFileInfo {
                type_,
                dialog_id,
                for_dark_theme,
                target,
            },
        );
    }

    /// `BackgroundManager::on_uploaded_background_file`
    /// (BackgroundManager.cpp:1058-1085) — the UploadBackgroundQuery answer.
    #[allow(clippy::too_many_arguments)]
    pub fn on_uploaded_background_file(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        file_upload_id: FileUploadId,
        type_: BackgroundType,
        dialog_id: DialogId,
        for_dark_theme: bool,
        wallpaper: telegram_api::WallPaper,
    ) {
        let Some(info) = self.pending_upload_queries.remove(&file_upload_id) else {
            // the C++ CHECK
            return;
        };
        let UploadedFileInfo {
            target,
            type_: _entry_type,
            ..
        } = info;

        let added_background = self.on_get_background(
            env,
            BackgroundId::new(0),
            "",
            Some(wallpaper),
            true,
            false,
            false,
        );
        let Some((background_id, added_type)) = added_background else {
            env.file_cancel_upload(file_upload_id);
            fail_upload_target(
                target,
                &Status::error(500, "Receive wrong uploaded background"),
            );
            return;
        };
        if added_type != type_ {
            tdutils::log_error!(
                "Type of uploaded background has changed from {type_:?} to {added_type:?}"
            );
        }

        let file_id = match self.backgrounds.get(&background_id) {
            Some(background) => background.file_id,
            None => {
                // the C++ CHECK
                return;
            }
        };
        if !file_id.is_valid() {
            env.file_cancel_upload(file_upload_id);
            fail_upload_target(
                target,
                &Status::error(500, "Receive wrong uploaded background without file"),
            );
            return;
        }
        if let tdutils::status::Result::Err(status) =
            env.file_merge(file_id, file_upload_id.file_id)
        {
            tdutils::log_error!("Failed to merge uploaded background file: {status}");
        }
        env.file_cancel_upload(file_upload_id);
        if !dialog_id.is_valid() {
            self.set_background_id(env, background_id, type_.clone(), for_dark_theme);
        }
        match target {
            UploadedBackgroundTarget::Plain { mut promise } => {
                promise.set_value(self.get_background_object(
                    env,
                    background_id,
                    for_dark_theme,
                    None,
                ));
            }
            UploadedBackgroundTarget::SetDialog {
                dialog_id,
                for_both,
                promise,
            } => {
                // the C++ set_dialog_background upload-promise lambda
                self.do_set_dialog_background(
                    env,
                    dialog_id,
                    background_id,
                    type_,
                    for_both,
                    promise,
                );
            }
        }
    }

    /// `UploadBackgroundQuery::on_error` (BackgroundManager.cpp:231-242)
    /// — the upload-query error half: the missing-parts branch is the C++
    /// TODO (inert), the else branch runs the partial-remote-location
    /// cleanup, the upload cancels, and the parked continuation (the C++
    /// handler's own `promise_`) fails.
    pub fn on_upload_background_query_error(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        file_upload_id: FileUploadId,
        status: Status,
    ) {
        assert!(file_upload_id.is_valid());
        assert!(status.is_error());
        if crate::files::get_missing_file_parts(&status).is_empty() {
            env.file_delete_partial_remote_location_if_needed(file_upload_id, &status);
        }
        env.file_cancel_upload(file_upload_id);
        let Some(info) = self.pending_upload_queries.remove(&file_upload_id) else {
            // the C++ CHECK
            return;
        };
        fail_upload_target(info.target, &status);
    }

    /// `BackgroundManager::remove_background` (BackgroundManager.cpp:1087-1110)
    /// — the `removeInstalledBackground` surface.
    pub fn remove_background(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        background_id: BackgroundId,
        mut promise: Promise<()>,
    ) {
        let Some(background) = self.backgrounds.get(&background_id) else {
            promise.set_error(Status::error(400, "Background not found"));
            return;
        };
        let type_ = background.type_.clone();

        if !type_.has_file() {
            if !background_id.is_local() {
                env.send_unsave_background_query(
                    background_id,
                    Some(telegram_api::InputWallPaper::inputWallPaperNoFile(
                        telegram_api::inputWallPaperNoFile {
                            id: background_id.get(),
                        },
                    )),
                    promise,
                );
            } else {
                promise.set_value(());
            }
            return;
        }

        let access_hash = background.access_hash;
        env.send_unsave_background_query(
            background_id,
            Some(telegram_api::InputWallPaper::inputWallPaper(
                telegram_api::inputWallPaper {
                    id: background_id.get(),
                    access_hash,
                },
            )),
            promise,
        );
    }

    /// `BackgroundManager::on_removed_background`
    /// (BackgroundManager.cpp:1112-1134) — the UnsaveBackgroundQuery answer.
    pub fn on_removed_background(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        background_id: BackgroundId,
        result: StatusResult<()>,
        mut promise: Promise<()>,
    ) {
        match result {
            StatusResult::Err(error) => {
                promise.set_error(error);
                return;
            }
            StatusResult::Ok(()) => {}
        }
        self.installed_backgrounds
            .retain(|background| background.0 != background_id);
        if background_id == self.set_background_id[0] {
            self.set_background_id(env, BackgroundId::new(0), BackgroundType::default(), false);
        }
        if background_id == self.set_background_id[1] {
            self.set_background_id(env, BackgroundId::new(0), BackgroundType::default(), true);
        }
        if background_id.is_local() {
            for for_dark_theme in [false, true] {
                let index = for_dark_theme as usize;
                if let Some(position) = self.local_background_ids[index]
                    .iter()
                    .position(|&id| id == background_id)
                {
                    self.local_background_ids[index].remove(position);
                    self.save_local_backgrounds(env, for_dark_theme);
                }
            }
        }
        promise.set_value(());
    }

    /// `BackgroundManager::reset_backgrounds` (BackgroundManager.cpp:1136-1143)
    /// — the `resetInstalledBackgrounds` surface.
    pub fn reset_backgrounds(&mut self, env: &mut impl BackgroundManagerEnv, promise: Promise<()>) {
        env.send_reset_backgrounds_query(promise);
    }

    /// `BackgroundManager::on_reset_background`
    /// (BackgroundManager.cpp:1145-1162) — the ResetBackgroundsQuery answer.
    pub fn on_reset_background(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        result: StatusResult<()>,
        mut promise: Promise<()>,
    ) {
        match result {
            StatusResult::Err(error) => {
                promise.set_error(error);
                return;
            }
            StatusResult::Ok(()) => {}
        }
        self.installed_backgrounds.clear();
        self.set_background_id(env, BackgroundId::new(0), BackgroundType::default(), false);
        self.set_background_id(env, BackgroundId::new(0), BackgroundType::default(), true);
        for for_dark_theme in [false, true] {
            let index = for_dark_theme as usize;
            if !self.local_background_ids[index].is_empty() {
                self.local_background_ids[index].clear();
                self.save_local_backgrounds(env, for_dark_theme);
            }
        }

        promise.set_value(());
    }

    /// `BackgroundManager::add_background` (BackgroundManager.cpp:1164-1245).
    fn add_background(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        background: &Background,
        replace_type: bool,
    ) {
        tdutils::log_info!("Add {} of {:?}", background.id.get(), background.type_);

        assert!(background.id.is_valid());
        let result = self.backgrounds.entry(background.id).or_default();

        let mut file_source_id = FileSourceId::new(0);
        if let Some((_, source_id)) = self.background_id_to_file_source_id.remove(&background.id) {
            assert!(!result.id.is_valid());
            file_source_id = source_id;
        }

        if !result.id.is_valid() {
            result.id = background.id;
            result.type_ = background.type_.clone();
        } else {
            assert_eq!(result.id, background.id);
            if replace_type {
                result.type_ = background.type_.clone();
            }
        }
        result.access_hash = background.access_hash;
        result.is_creator = background.is_creator;
        result.is_default = background.is_default;
        result.is_dark = background.is_dark;

        if result.name != background.name {
            if !result.name.is_empty() {
                tdutils::log_error!(
                    "Background name has changed from {} to {}",
                    result.name,
                    background.name
                );
                // keep correspondence from previous name to background ID
                // it will not harm, because background names can't be reassigned
            }

            result.name = background.name.clone();

            if !BackgroundType::is_background_name_local(&result.name) {
                self.name_to_background_id
                    .insert(result.name.clone(), result.id);
                self.loaded_from_database_backgrounds.remove(&result.name); // don't needed anymore
            }
        }

        if result.file_id != background.file_id {
            if result.file_id.is_valid() {
                let old_main_file_id = env.file_view(result.file_id).map(|view| view.main_file_id);
                let new_main_file_id = env
                    .file_view(background.file_id)
                    .map(|view| view.main_file_id);
                if !background.file_id.is_valid() || old_main_file_id != new_main_file_id {
                    tdutils::log_error!(
                        "Background file has changed from {:?} to {:?}",
                        result.file_id,
                        background.file_id
                    );
                    self.file_id_to_background_id.remove(&result.file_id);
                    result.file_source_id = FileSourceId::new(0);
                }
                assert!(!file_source_id.is_valid());
            }
            if file_source_id.is_valid() {
                result.file_source_id = file_source_id;
            }

            result.file_id = background.file_id;

            if result.file_id.is_valid() {
                if !result.file_source_id.is_valid() {
                    result.file_source_id =
                        env.create_background_file_source(result.id, result.access_hash);
                }
                let file_source_id = result.file_source_id;
                for file_id in env.document_file_ids(result.file_id) {
                    env.file_add_source(file_id, file_source_id, "add_background");
                }

                self.file_id_to_background_id
                    .insert(result.file_id, result.id);
            }
        } else {
            // if file_source_id is valid, then this is a new background with
            // result->file_id == FileId() — then background.file_id ==
            // FileId(), then this is a fill background, which can't have
            // file_source_id
            assert!(!file_source_id.is_valid());
        }
    }

    /// `BackgroundManager::get_background` (BackgroundManager.cpp:1247-1263).
    fn get_background(&self, background_id: BackgroundId) -> Option<&Background> {
        self.backgrounds.get(&background_id)
    }

    /// `BackgroundManager::get_background_name_database_key`
    /// (BackgroundManager.cpp:1265-1267).
    fn get_background_name_database_key(name: &str) -> Vec<u8> {
        let mut key = b"bgn".to_vec();
        key.extend_from_slice(name.as_bytes());
        key
    }

    /// `BackgroundManager::on_get_background`
    /// (BackgroundManager.cpp:1269-1366) — the wallPaper intake, also the
    /// `ThemeManager`/`BackgroundInfo` half.
    #[allow(clippy::too_many_arguments)]
    pub fn on_get_background(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        expected_background_id: BackgroundId,
        expected_background_name: &str,
        wallpaper: Option<telegram_api::WallPaper>,
        replace_type: bool,
        allow_empty: bool,
        is_no_file_pattern: bool,
    ) -> Option<(BackgroundId, BackgroundType)> {
        let wallpaper = match wallpaper {
            Some(wallpaper) => wallpaper,
            None => {
                if !allow_empty {
                    tdutils::log_error!("Receive unexpected empty background");
                }
                return None;
            }
        };

        match wallpaper {
            telegram_api::WallPaper::wallPaperNoFile(wallpaper) => {
                let Some(settings) = wallpaper.settings else {
                    if !allow_empty {
                        tdutils::log_error!("Receive wallPaperNoFile without settings");
                    }
                    return None;
                };

                let background_id = BackgroundId::new(wallpaper.id);
                if background_id.is_local() {
                    tdutils::log_error!("Receive an invalid no-file wallpaper id");
                    return None;
                }

                let mut background = Background {
                    id: background_id,
                    is_default: wallpaper.default,
                    is_dark: wallpaper.dark,
                    type_: BackgroundType::from_settings(true, is_no_file_pattern, Some(&settings)),
                    ..Background::default()
                };
                background.name = background.type_.get_link(true);
                if !background.id.is_valid() {
                    self.set_local_background_id(env, &mut background);
                }
                let background_id = background.id;
                let background_type = background.type_.clone();
                self.add_background(env, &background, replace_type);

                Some((background_id, background_type))
            }
            telegram_api::WallPaper::wallPaper(wallpaper) => {
                let background_id = BackgroundId::new(wallpaper.id);
                if !background_id.is_valid()
                    || background_id.is_local()
                    || BackgroundType::is_background_name_local(&wallpaper.slug)
                {
                    tdutils::log_error!("Receive an invalid wallpaper id");
                    return None;
                }
                if expected_background_id.is_valid() && background_id != expected_background_id {
                    tdutils::log_error!("Receive a wallpaper with unexpected id");
                }

                let Some(document) = wallpaper.document else {
                    tdutils::log_error!("Receive a wallpaper without a document");
                    return None;
                };
                let Some(file_id) = env.on_get_document(
                    document,
                    if wallpaper.pattern {
                        crate::documents_manager::Subtype::Pattern
                    } else {
                        crate::documents_manager::Subtype::Background
                    },
                ) else {
                    tdutils::log_error!("Receive wrong document in a wallpaper");
                    return None;
                };

                let background = Background {
                    id: background_id,
                    access_hash: wallpaper.access_hash,
                    is_creator: wallpaper.creator,
                    is_default: wallpaper.default,
                    is_dark: wallpaper.dark,
                    type_: BackgroundType::from_settings(
                        false,
                        wallpaper.pattern,
                        wallpaper.settings.as_ref(),
                    ),
                    name: wallpaper.slug,
                    file_id,
                    ..Background::default()
                };
                let background_id = background.id;
                let background_type = background.type_.clone();
                self.add_background(env, &background, replace_type);

                if !expected_background_name.is_empty()
                    && background.name != expected_background_name
                {
                    tdutils::log_error!(
                        "Expected background {expected_background_name}, but receive {}",
                        background.name
                    );
                    self.name_to_background_id
                        .insert(expected_background_name.to_string(), background_id);
                }

                if env.use_sqlite_pmc() {
                    tdutils::log_info!(
                        "Save {background_id:?} to database with name {}",
                        background.name
                    );
                    assert!(!BackgroundType::is_background_name_local(&background.name));
                    let mut out = Vec::new();
                    store_parse::store_log_event_version(&mut out);
                    background.store(env, &mut out);
                    env.sqlite_pmc_set_raw(
                        &Self::get_background_name_database_key(&background.name),
                        &out,
                    );
                }

                Some((background_id, background_type))
            }
        }
    }

    /// `BackgroundManager::get_background_object`
    /// (BackgroundManager.cpp:1406-1427).
    pub fn get_background_object(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        background_id: BackgroundId,
        for_dark_theme: bool,
        type_: Option<&BackgroundType>,
    ) -> Option<td_api::Background> {
        let background = self.backgrounds.get(&background_id)?;
        let index = for_dark_theme as usize;
        let mut resolved_type = type_;
        if resolved_type.is_none() {
            resolved_type = Some(&background.type_);
            // first check another set_background_id to get correct type if
            // both backgrounds are the same
            if background_id == self.set_background_id[1 - index] {
                resolved_type = Some(&self.set_background_type[1 - index]);
            }
            if background_id == self.set_background_id[index] {
                resolved_type = Some(&self.set_background_type[index]);
            }
        }
        let resolved_type = resolved_type.expect("resolved background type");
        Some(td_api::Background {
            id: background.id.get(),
            is_default: background.is_default,
            is_dark: background.is_dark,
            name: background.name.clone(),
            document: env.get_document_object(background.file_id, PhotoFormat::Png),
            r#type: Some(resolved_type.get_background_type_object()),
        })
    }

    /// `BackgroundManager::get_backgrounds_object`
    /// (BackgroundManager.cpp:1429-1465).
    fn get_backgrounds_object(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        for_dark_theme: bool,
    ) -> td_api::Backgrounds {
        let index = for_dark_theme as usize;
        let installed_backgrounds = self.installed_backgrounds.clone();
        let local_background_ids = self.local_background_ids[index].clone();
        let mut backgrounds: Vec<td_api::Background> = Vec::new();
        for (background_id, background_type) in &installed_backgrounds {
            if let Some(background) = self.get_background_object(
                env,
                *background_id,
                for_dark_theme,
                Some(background_type),
            ) {
                backgrounds.push(background);
            }
        }
        let background_id = self.set_background_id[index];
        let have_background = installed_backgrounds
            .iter()
            .any(|background| background_id == background.0);
        if background_id.is_valid()
            && !have_background
            && let Some(background) =
                self.get_background_object(env, background_id, for_dark_theme, None)
        {
            backgrounds.push(background);
        }
        for local_background_id in &local_background_ids {
            if *local_background_id != background_id
                && let Some(background) =
                    self.get_background_object(env, *local_background_id, for_dark_theme, None)
            {
                backgrounds.push(background);
            }
        }
        backgrounds.sort_by_key(|background| {
            if background.id == background_id.get() {
                0
            } else {
                let theme_score = i32::from(background.is_dark != for_dark_theme);
                let local_score = if BackgroundId::new(background.id).is_local() {
                    0
                } else {
                    2
                };
                1 + local_score + theme_score
            }
        });
        td_api::Backgrounds { backgrounds }
    }

    /// `BackgroundManager::get_background_file_source_id`
    /// (BackgroundManager.cpp:1467-1489).
    pub fn get_background_file_source_id(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        background_id: BackgroundId,
        access_hash: i64,
    ) -> FileSourceId {
        if !background_id.is_valid() {
            return FileSourceId::new(0);
        }

        if let Some(background) = self.backgrounds.get_mut(&background_id) {
            if !background.file_source_id.is_valid() {
                background.file_source_id =
                    env.create_background_file_source(background_id, background.access_hash);
            }
            return background.file_source_id;
        }

        let entry = self
            .background_id_to_file_source_id
            .entry(background_id)
            .or_insert((0, FileSourceId::new(0)));
        if entry.0 == 0 {
            entry.0 = access_hash;
        }
        if !entry.1.is_valid() {
            entry.1 = env.create_background_file_source(background_id, entry.0);
        }
        entry.1
    }

    /// `BackgroundManager::get_current_state`
    /// (BackgroundManager.cpp:1491-1498) — the Requests.cpp:2211 block.
    pub fn get_current_state(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        if env.is_bot() {
            return;
        }

        updates.push(self.get_update_default_background_object(env, false));
        updates.push(self.get_update_default_background_object(env, true));
    }

    /// `BackgroundManager::store_background` (BackgroundManager.cpp:480-490)
    /// — the `ThemeManagerEnv::store_background_row` seam; `false` models
    /// the unresolvable row (the theme stores background-less).
    pub fn store_background(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        background_id: BackgroundId,
        out: &mut Vec<u8>,
    ) -> bool {
        let Some(background) = self.get_background(background_id) else {
            return false;
        };
        let background = background.clone();
        background.store(env, out);
        true
    }

    /// `BackgroundManager::parse_background` (BackgroundManager.cpp:492-507)
    /// — the `ThemeManagerEnv::parse_background_row` seam.
    pub fn parse_background(
        &mut self,
        env: &mut impl BackgroundManagerEnv,
        parser: &mut Parser<'_>,
    ) -> std::result::Result<BackgroundId, &'static str> {
        let background = Background::parse(env, parser)?;
        if !background.has_new_local_id
            || background.file_id.is_valid() != background.type_.has_file()
            || !background.id.is_valid()
        {
            return std::result::Result::Err("Failed to load background");
        }
        if background.id.is_local()
            && !background.type_.has_file()
            && background.id.get() > self.max_local_background_id.get()
        {
            self.set_max_local_background_id(env, background.id);
        }
        let background_id = background.id;
        self.add_local_background_to_cache(&background);
        self.add_background(env, &background, false);
        std::result::Result::Ok(background_id)
    }
}

/// The failed-upload continuation answer (the C++ `UploadedFileInfo`
/// promise error half).
fn fail_upload_target(target: UploadedBackgroundTarget, error: &Status) {
    match target {
        UploadedBackgroundTarget::Plain { mut promise } => {
            promise.set_error(error.clone());
        }
        UploadedBackgroundTarget::SetDialog { mut promise, .. } => {
            promise.set_error(error.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::documents_manager::Subtype;
    use tdutils::promise::PromiseInterface;

    /// The hermetic env: pmc/sqlite maps, a documents registry, a file-view
    /// table and query logs; the seam sends log a line (the defaults do).
    struct TestEnv {
        is_bot: bool,
        closed: bool,
        pmc: HashMap<Vec<u8>, Vec<u8>>,
        use_sqlite: bool,
        sqlite: HashMap<Vec<u8>, Vec<u8>>,
        updates: Vec<td_api::Update>,
        file_views: HashMap<FileId, FileViewInfo>,
        file_sources: HashMap<(i64, i64), FileSourceId>,
        next_file_source_id: i32,
        next_internal_upload_id: i64,
        uploaded: Vec<FileUploadId>,
        deleted_partial: Vec<FileUploadId>,
    }

    impl TestEnv {
        fn new() -> TestEnv {
            TestEnv {
                is_bot: false,
                closed: false,
                pmc: HashMap::new(),
                use_sqlite: false,
                sqlite: HashMap::new(),
                updates: Vec::new(),
                file_views: HashMap::new(),
                file_sources: HashMap::new(),
                next_file_source_id: 1,
                next_internal_upload_id: 0,
                uploaded: Vec::new(),
                deleted_partial: Vec::new(),
            }
        }
    }

    impl BackgroundManagerEnv for TestEnv {
        fn is_bot(&self) -> bool {
            self.is_bot
        }

        fn close_status(&self) -> Status {
            if self.closed {
                crate::config_result::request_aborted_error()
            } else {
                Status::ok()
            }
        }

        fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
            self.pmc.get(key).cloned().unwrap_or_default()
        }

        fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
            self.pmc.insert(key.to_vec(), value.to_vec());
        }

        fn binlog_pmc_erase(&mut self, key: &str) {
            self.pmc.remove(key.as_bytes());
        }

        fn use_sqlite_pmc(&self) -> bool {
            self.use_sqlite
        }

        fn sqlite_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
            self.sqlite.get(key).cloned().unwrap_or_default()
        }

        fn sqlite_pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
            self.sqlite.insert(key.to_vec(), value.to_vec());
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn on_get_document(
            &mut self,
            document: telegram_api::Document,
            _document_subtype: Subtype,
        ) -> Option<FileId> {
            let telegram_api::Document::document(document) = document else {
                return None;
            };
            Some(FileId::new(document.id as i32, 0))
        }

        fn get_document_object(
            &mut self,
            file_id: FileId,
            _thumbnail_format: PhotoFormat,
        ) -> Option<td_api::Document> {
            if !file_id.is_valid() {
                return None;
            }
            Some(td_api::Document {
                file_name: "background.jpg".to_string(),
                mime_type: "image/jpeg".to_string(),
                minithumbnail: None,
                thumbnail: None,
                document: None,
            })
        }

        fn store_document_row(&mut self, file_id: FileId, out: &mut Vec<u8>) {
            // the interim row codec: the bare id (the documents row codec
            // is the DialogPhotoStoreEnv'd real one in td.rs)
            store_parse::store_i32(out, file_id.get());
        }

        fn parse_document_row(
            &mut self,
            parser: &mut Parser<'_>,
        ) -> std::result::Result<FileId, &'static str> {
            Ok(FileId::new(parser.i32_()?, 0))
        }

        fn get_input_file_id(
            &mut self,
            _type_: FileType,
            _file: Option<&td_api::InputFile>,
        ) -> StatusResult<FileId> {
            StatusResult::Ok(FileId::new(1, 0))
        }

        fn file_view(&mut self, file_id: FileId) -> Option<FileViewInfo> {
            self.file_views.get(&file_id).copied()
        }

        fn file_merge(&mut self, _x: FileId, _y: FileId) -> StatusResult<()> {
            StatusResult::Ok(())
        }

        fn file_cancel_upload(&mut self, file_upload_id: FileUploadId) {
            self.uploaded.push(file_upload_id);
        }

        fn file_add_source(&mut self, _file_id: FileId, _source_id: FileSourceId, _source: &str) {}

        fn document_file_ids(&mut self, file_id: FileId) -> Vec<FileId> {
            vec![file_id]
        }

        fn create_background_file_source(
            &mut self,
            background_id: BackgroundId,
            access_hash: i64,
        ) -> FileSourceId {
            *self
                .file_sources
                .entry((background_id.get(), access_hash))
                .or_insert_with(|| {
                    let source_id = FileSourceId::new(self.next_file_source_id);
                    self.next_file_source_id += 1;
                    source_id
                })
        }

        fn get_internal_upload_id(&mut self) -> i64 {
            self.next_internal_upload_id += 1;
            self.next_internal_upload_id
        }

        fn check_dialog_access(
            &mut self,
            dialog_id: DialogId,
            _allow_secret_chats: bool,
            _access_rights: AccessRights,
            _source: &str,
        ) -> StatusResult<()> {
            if dialog_id.is_valid() {
                StatusResult::Ok(())
            } else {
                StatusResult::from_status(Status::error(400, "Chat not found"))
            }
        }

        fn get_channel_permissions(
            &mut self,
            _channel_id: ChannelId,
        ) -> Option<DialogParticipantStatus> {
            None
        }

        fn get_secret_chat_user_id(&mut self, _secret_chat_id: SecretChatId) -> UserId {
            UserId::new(0)
        }

        fn start_background_file_upload(&mut self, file_upload_id: FileUploadId) {
            self.uploaded.push(file_upload_id);
        }

        fn file_delete_partial_remote_location_if_needed(
            &mut self,
            file_upload_id: FileUploadId,
            _status: &Status,
        ) {
            self.deleted_partial.push(file_upload_id);
        }
    }

    /// The promise capture for tests.
    struct Capture<T: Send> {
        result: std::sync::Arc<std::sync::Mutex<Option<StatusResult<T>>>>,
    }

    impl<T: Send> PromiseInterface<T> for Capture<T> {
        fn set_value(&mut self, value: T) {
            *self.result.lock().unwrap() = Some(StatusResult::Ok(value));
        }

        fn set_error(&mut self, error: Status) {
            *self.result.lock().unwrap() = Some(StatusResult::from_status(error));
        }
    }

    type CaptureSlot<T> = std::sync::Arc<std::sync::Mutex<Option<StatusResult<T>>>>;

    fn capture<T: Send + 'static>() -> (Promise<T>, CaptureSlot<T>) {
        let result = std::sync::Arc::new(std::sync::Mutex::new(None));
        let promise = Promise::from_interface(Capture {
            result: std::sync::Arc::clone(&result),
        });
        (promise, result)
    }

    fn captured<T: Send>(slot: &CaptureSlot<T>) -> Option<StatusResult<T>> {
        slot.lock().unwrap().take()
    }

    fn captured_ok<T: Send>(slot: &CaptureSlot<T>) -> T {
        match captured(slot).expect("unresolved promise") {
            StatusResult::Ok(value) => value,
            StatusResult::Err(error) => panic!("expected ok, got {error}"),
        }
    }

    fn captured_err<T: Send>(slot: &CaptureSlot<T>) -> Status {
        match captured(slot).expect("unresolved promise") {
            StatusResult::Err(error) => error,
            StatusResult::Ok(_) => panic!("expected error"),
        }
    }

    fn wallpaper_no_file(
        id: i64,
        settings: telegram_api::WallPaperSettings,
    ) -> telegram_api::WallPaper {
        telegram_api::WallPaper::wallPaperNoFile(telegram_api::wallPaperNoFile {
            id,
            flags: 1 | (1 << 2),
            default: false,
            dark: false,
            settings: Some(settings),
        })
    }

    fn solid_fill_settings(color: i32) -> telegram_api::WallPaperSettings {
        telegram_api::WallPaperSettings {
            flags: 1,
            blur: false,
            motion: false,
            background_color: color,
            second_background_color: color,
            third_background_color: 0,
            fourth_background_color: 0,
            intensity: 0,
            rotation: 0,
            emoticon: String::new(),
        }
    }

    fn wallpaper(id: i64, document_id: i64, pattern: bool) -> telegram_api::WallPaper {
        // the id must be non-local (> 0x7FFFFFFF, the C++ wallPaper intake
        // rejects local ids) and the slug must pass
        // `is_background_name_local` (longer than 13 characters, base64url
        // alphabet only — no underscores)
        telegram_api::WallPaper::wallPaper(telegram_api::wallPaper {
            id,
            flags: 1 | (1 << 3),
            creator: false,
            default: false,
            pattern,
            dark: false,
            access_hash: 123,
            slug: format!("alongslugfor{id}"),
            document: Some(telegram_api::Document::document(telegram_api::document {
                flags: 0,
                id: document_id,
                access_hash: 456,
                file_reference: Vec::new(),
                date: 1,
                mime_type: "image/jpeg".to_string(),
                size: 100,
                thumbs: Vec::new(),
                video_thumbs: Vec::new(),
                dc_id: 2,
                attributes: Vec::new(),
            })),
            settings: Some(solid_fill_settings(0xABCDEF)),
        })
    }

    fn background_id_of(background: &Option<td_api::Background>) -> i64 {
        background
            .as_ref()
            .map(|background| background.id)
            .unwrap_or(0)
    }

    fn local_file_input() -> td_api::InputBackground {
        td_api::InputBackground::inputBackgroundLocal(td_api::inputBackgroundLocal {
            background: Some(td_api::InputFile::inputFileLocal(td_api::inputFileLocal {
                path: "background.jpg".to_string(),
            })),
        })
    }

    fn fill_type(color: i32) -> td_api::BackgroundType {
        td_api::BackgroundType::backgroundTypeFill(td_api::backgroundTypeFill {
            fill: Some(td_api::BackgroundFill::backgroundFillSolid(
                td_api::backgroundFillSolid { color },
            )),
        })
    }

    fn wallpaper_type() -> td_api::BackgroundType {
        td_api::BackgroundType::backgroundTypeWallpaper(td_api::backgroundTypeWallpaper {
            is_blurred: false,
            is_moving: false,
        })
    }

    #[test]
    fn background_fill_links_round_trip() {
        let gradient = BackgroundFill::gradient(0x12AABB, 0xCCDDEE, 90);
        let parsed =
            BackgroundFill::get_background_fill_from_name(&gradient.get_link(true)).unwrap();
        assert_eq!(gradient, parsed);

        let solid = BackgroundFill::solid(0x112233);
        let parsed = BackgroundFill::get_background_fill_from_name(&solid.get_link(true)).unwrap();
        assert_eq!(solid, parsed);

        let freeform = BackgroundFill::freeform(0x112233, 0x445566, 0x778899, 0xAABBCC);
        let parsed =
            BackgroundFill::get_background_fill_from_name(&freeform.get_link(true)).unwrap();
        assert_eq!(freeform, parsed);

        let three_color = BackgroundFill::freeform(0x112233, 0x445566, 0x778899, -1);
        let parsed =
            BackgroundFill::get_background_fill_from_name(&three_color.get_link(true)).unwrap();
        assert_eq!(three_color, parsed);
        assert_eq!(parsed.get_type(), BackgroundFillType::FreeformGradient);

        // the '~' two-color form is a gradient
        let parsed = BackgroundFill::get_background_fill_from_name("ffffff~000000").unwrap();
        assert_eq!(parsed.get_type(), BackgroundFillType::Gradient);
        assert_eq!(parsed.top_color, 0xFFFFFF);
        assert_eq!(parsed.bottom_color, 0);

        // invalid
        assert!(BackgroundFill::get_background_fill_from_name("zzzzzz").is_err());
        assert!(BackgroundFill::get_background_fill_from_name("").is_err());
        assert!(BackgroundFill::get_background_fill_from_name("1234567").is_err());
    }

    #[test]
    fn background_fill_is_dark() {
        assert!(!BackgroundFill::solid(0x808080).is_dark());
        assert!(BackgroundFill::solid(0x001122).is_dark());
        assert!(BackgroundFill::gradient(0x101010, 0x202020, 0).is_dark());
        assert!(!BackgroundFill::gradient(0xFFFFFF, 0x101010, 0).is_dark());
        assert!(BackgroundFill::freeform(0x101010, 0x202020, 0x303030, -1).is_dark());
        assert!(!BackgroundFill::freeform(0x101010, 0x202020, 0x303030, 0xF0F0F0).is_dark());
    }

    #[test]
    fn background_fill_from_td_api_validates() {
        assert!(BackgroundFill::get_background_fill(None).is_err());
        let invalid_color =
            td_api::BackgroundFill::backgroundFillSolid(td_api::backgroundFillSolid {
                color: 0x1000000,
            });
        assert!(BackgroundFill::get_background_fill(Some(&invalid_color)).is_err());
        let invalid_rotation =
            td_api::BackgroundFill::backgroundFillGradient(td_api::backgroundFillGradient {
                top_color: 0,
                bottom_color: 0,
                rotation_angle: 10,
            });
        assert!(BackgroundFill::get_background_fill(Some(&invalid_rotation)).is_err());
        let wrong_count = td_api::BackgroundFill::backgroundFillFreeformGradient(
            td_api::backgroundFillFreeformGradient { colors: vec![1, 2] },
        );
        assert!(BackgroundFill::get_background_fill(Some(&wrong_count)).is_err());
    }

    #[test]
    fn background_type_links_round_trip() {
        let fill_type = BackgroundType::fill(BackgroundFill::gradient(0x112233, 0x445566, 45), 0);
        let parsed = BackgroundType::get_local_background_type(&fill_type.get_link(true)).unwrap();
        assert_eq!(fill_type, parsed);

        let mut pattern =
            BackgroundType::pattern(true, BackgroundFill::gradient(0x112233, 0x445566, 45), -50);
        pattern.apply_parameters_from_link(
            "112233-445566?rotation=45&intensity=50&bg_color=112233-445566&rotation=45&mode=blur+motion",
        );
        assert!(pattern.is_moving);
        assert!(!pattern.is_blurred); // blur does not apply to patterns
        assert_eq!(pattern.intensity, 50);
        assert_eq!(pattern.get_dark_theme_dimming(), 0); // patterns answer 0

        // the wallpaper mode applies
        let mut wallpaper_type = BackgroundType::wallpaper(false, false, 60);
        wallpaper_type.apply_parameters_from_link("slug?mode=blur+motion");
        assert!(wallpaper_type.is_blurred);
        assert!(wallpaper_type.is_moving);
        assert_eq!(wallpaper_type.get_dark_theme_dimming(), 60);

        // invalid intensity falls back to 50
        let mut pattern_type = BackgroundType::pattern(true, BackgroundFill::solid(1), 0);
        pattern_type.apply_parameters_from_link("slug?intensity=150");
        assert_eq!(pattern_type.intensity, 50);
    }

    #[test]
    fn is_background_name_local_cases() {
        assert!(BackgroundType::is_background_name_local(""));
        assert!(BackgroundType::is_background_name_local("112233-445566"));
        assert!(BackgroundType::is_background_name_local(
            "slug?intensity=50"
        )); // '?' at 4
        assert!(!BackgroundType::is_background_name_local(
            "long_slug_with_underscore"
        ));
        assert!(BackgroundType::is_background_name_local("short"));
        assert!(BackgroundType::is_background_name_local("with space"));
    }

    #[test]
    fn get_background_type_from_td_api() {
        assert_eq!(
            BackgroundType::get_background_type(None, 0).unwrap(),
            BackgroundType::default()
        );
        // the C++ answers the default type for a null input BEFORE the
        // dark-theme-dimming check (BackgroundType.cpp:356-358)
        assert_eq!(
            BackgroundType::get_background_type(None, 101).unwrap(),
            BackgroundType::default()
        );

        let wallpaper_type = BackgroundType::get_background_type(
            Some(&td_api::BackgroundType::backgroundTypeWallpaper(
                td_api::backgroundTypeWallpaper {
                    is_blurred: true,
                    is_moving: false,
                },
            )),
            70,
        )
        .unwrap();
        assert!(wallpaper_type.has_file());
        assert!(wallpaper_type.is_blurred);
        assert_eq!(wallpaper_type.get_dark_theme_dimming(), 70);

        let inverted_pattern = BackgroundType::get_background_type(
            Some(&td_api::BackgroundType::backgroundTypePattern(
                td_api::backgroundTypePattern {
                    fill: Some(td_api::BackgroundFill::backgroundFillSolid(
                        td_api::backgroundFillSolid { color: 5 },
                    )),
                    intensity: 60,
                    is_inverted: true,
                    is_moving: false,
                },
            )),
            0,
        )
        .unwrap();
        assert_eq!(inverted_pattern.intensity, -60);
        assert_eq!(
            inverted_pattern.get_background_type_object(),
            td_api::BackgroundType::backgroundTypePattern(td_api::backgroundTypePattern {
                fill: Some(td_api::BackgroundFill::backgroundFillSolid(
                    td_api::backgroundFillSolid { color: 5 },
                )),
                intensity: 60,
                is_inverted: true,
                is_moving: false,
            })
        );

        let chat_theme = BackgroundType::get_background_type(
            Some(&td_api::BackgroundType::backgroundTypeChatTheme(
                td_api::backgroundTypeChatTheme {
                    theme_name: "abc".to_string(),
                },
            )),
            0,
        )
        .unwrap();
        assert!(!chat_theme.has_file());
        assert_eq!(
            chat_theme.get_background_type_object(),
            td_api::BackgroundType::backgroundTypeChatTheme(td_api::backgroundTypeChatTheme {
                theme_name: "abc".to_string(),
            })
        );
    }

    #[test]
    fn background_row_store_parse_round_trip() {
        let mut env = TestEnv::new();
        let background = Background {
            id: BackgroundId::new(7),
            access_hash: 8,
            name: "slug".to_string(),
            file_id: FileId::new(42, 0),
            is_creator: true,
            is_default: false,
            is_dark: true,
            has_new_local_id: true,
            type_: BackgroundType::wallpaper(true, false, 30),
            file_source_id: FileSourceId::new(3),
        };
        let mut out = Vec::new();
        background.store(&mut env, &mut out);

        let mut parser = Parser::new(&out);
        let parsed = Background::parse(&mut env, &mut parser).unwrap();
        assert_eq!(background.id, parsed.id);
        assert_eq!(background.access_hash, parsed.access_hash);
        assert_eq!(background.name, parsed.name);
        assert_eq!(background.file_id, parsed.file_id);
        assert_eq!(background.is_creator, parsed.is_creator);
        assert_eq!(background.is_dark, parsed.is_dark);
        assert_eq!(background.type_, parsed.type_);
        // the file_source_id is not part of the row
        assert!(!parsed.file_source_id.is_valid());

        // a background without a file stores the no-document shape
        let fill_background = Background {
            id: BackgroundId::new(3_000_000_999),
            name: "ffffff".to_string(),
            is_creator: true,
            has_new_local_id: true,
            type_: BackgroundType::fill(BackgroundFill::solid(0xFFFFFF), 0),
            ..Background::default()
        };
        let mut out = Vec::new();
        fill_background.store(&mut env, &mut out);
        let mut parser = Parser::new(&out);
        let parsed = Background::parse(&mut env, &mut parser).unwrap();
        assert!(!parsed.file_id.is_valid());
        assert_eq!(parsed.type_, fill_background.type_);
    }

    #[test]
    fn on_get_background_adds_and_builds_objects() {
        let mut env = TestEnv::new();
        let mut manager = BackgroundManager::new();

        // wallPaperNoFile half: the id-0 wallpaper gets a local id
        let pair = manager
            .on_get_background(
                &mut env,
                BackgroundId::new(0),
                "",
                Some(wallpaper_no_file(0, solid_fill_settings(0xEEEEEE))),
                false,
                false,
                false,
            )
            .unwrap();
        assert!(pair.0.is_local());
        assert!(!pair.1.has_file());

        // wallPaper half
        let pair = manager
            .on_get_background(
                &mut env,
                BackgroundId::new(0),
                "",
                Some(wallpaper(3_000_000_100, 5, false)),
                false,
                false,
                false,
            )
            .unwrap();
        assert_eq!(pair.0, BackgroundId::new(3_000_000_100));
        assert!(pair.1.has_file());

        let background = manager
            .get_background_object(&mut env, BackgroundId::new(3_000_000_100), false, None)
            .unwrap();
        assert_eq!(background.id, 3_000_000_100);
        assert_eq!(background.name, "alongslugfor3000000100");
        assert!(background.document.is_some());
        assert!(matches!(
            background.r#type,
            Some(td_api::BackgroundType::backgroundTypeWallpaper(_))
        ));

        // invalid wallpapers
        assert!(
            manager
                .on_get_background(
                    &mut env,
                    BackgroundId::new(0),
                    "",
                    None,
                    false,
                    false,
                    false
                )
                .is_none()
        );
        assert!(
            manager
                .on_get_background(
                    &mut env,
                    BackgroundId::new(0),
                    "",
                    Some(wallpaper(0, 5, false)),
                    false,
                    false,
                    false,
                )
                .is_none()
        ); // invalid id
        assert!(
            manager
                .on_get_background(
                    &mut env,
                    BackgroundId::new(0),
                    "",
                    Some(wallpaper(5, 5, false)),
                    false,
                    false,
                    false,
                )
                .is_none()
        ); // local id
        assert!(
            manager
                .on_get_background(
                    &mut env,
                    BackgroundId::new(0),
                    "",
                    Some(telegram_api::WallPaper::wallPaperNoFile(
                        telegram_api::wallPaperNoFile {
                            id: 50,
                            flags: 0,
                            default: false,
                            dark: false,
                            settings: None,
                        }
                    )),
                    false,
                    false,
                    false,
                )
                .is_none()
        ); // settings-less

        // the name cache answers a second search
        let (promise, slot) = capture();
        manager.search_background(&mut env, "alongslugfor3000000100", promise);
        let background = captured_ok(&slot).unwrap();
        assert_eq!(background.id, 3_000_000_100);
    }

    #[test]
    fn search_background_paths() {
        let mut env = TestEnv::new();
        let mut manager = BackgroundManager::new();

        // empty name
        let (promise, slot) = capture();
        manager.search_background(&mut env, "", promise);
        let error = captured_err(&slot);
        assert_eq!(error.code(), 400);
        assert_eq!(error.message(), "Background name must be non-empty");

        // local background
        let (promise, slot) = capture();
        manager.search_background(&mut env, "112233-445566?rotation=45", promise);
        let background = captured_ok(&slot).unwrap();
        assert!(BackgroundId::new(background.id).is_local());
        assert!(background.document.is_none());
        assert!(matches!(
            background.r#type,
            Some(td_api::BackgroundType::backgroundTypeFill(_))
        ));

        // the local background id was persisted
        let max_bg_id = std::str::from_utf8(&env.pmc[b"max_bg_id".as_slice()])
            .unwrap()
            .parse::<i64>()
            .unwrap();
        assert_eq!(max_bg_id, background.id);

        // invalid local background
        let (promise, slot) = capture();
        manager.search_background(&mut env, "zzzzzz", promise);
        let error = captured_err(&slot);
        assert_eq!(error.code(), 400);
        assert_eq!(error.message(), "WALLPAPER_INVALID");

        // remote slug parks the net promise
        let (promise, _slot) = capture();
        manager.search_background(&mut env, "remoteslugwithbase64", promise);

        // the sqlite path: the value loads and the first search answers 404
        env.use_sqlite = true;
        let mut out = Vec::new();
        store_parse::store_log_event_version(&mut out);
        Background {
            id: BackgroundId::new(3_000_000_077),
            access_hash: 1,
            name: "dbslugforseventy7".to_string(),
            file_id: FileId::new(9, 0),
            type_: BackgroundType::wallpaper(false, false, 0),
            has_new_local_id: true,
            ..Background::default()
        }
        .store(&mut env, &mut out);
        env.sqlite.insert(b"bgndbslugforseventy7".to_vec(), out);

        let (promise, slot) = capture();
        manager.search_background(&mut env, "dbslugforseventy7", promise);
        assert!(captured_ok(&slot).is_none()); // the first-search 404

        let (promise, slot) = capture();
        manager.search_background(&mut env, "dbslugforseventy7", promise);
        let background = captured_ok(&slot).unwrap();
        assert_eq!(background.id, 3_000_000_077);
    }

    #[test]
    fn set_background_local_and_remote() {
        let mut env = TestEnv::new();
        let mut manager = BackgroundManager::new();

        // local fill background
        let (promise, slot) = capture();
        manager.set_background(&mut env, None, Some(&fill_type(0xFFFFFF)), false, promise);
        let background = captured_ok(&slot).unwrap();
        assert!(BackgroundId::new(background.id).is_local());
        assert_eq!(env.updates.len(), 1); // updateDefaultBackground sent
        assert!(matches!(
            &env.updates[0],
            td_api::Update::updateDefaultBackground(td_api::updateDefaultBackground {
                for_dark_theme: false,
                ..
            })
        ));

        // chat theme type is unsupported for local creation
        let chat_theme_type =
            td_api::BackgroundType::backgroundTypeChatTheme(td_api::backgroundTypeChatTheme {
                theme_name: "abc".to_string(),
            });
        let (promise, slot) = capture();
        manager.set_background(&mut env, None, Some(&chat_theme_type), false, promise);
        assert_eq!(captured_err(&slot).code(), 400);

        // previous background is rejected
        let previous =
            td_api::InputBackground::inputBackgroundPrevious(td_api::inputBackgroundPrevious {
                message_id: 5,
            });
        let (promise, slot) = capture();
        manager.set_background(
            &mut env,
            Some(&previous),
            Some(&fill_type(5)),
            false,
            promise,
        );
        assert_eq!(captured_err(&slot).code(), 400);

        // unknown remote background
        let remote =
            td_api::InputBackground::inputBackgroundRemote(td_api::inputBackgroundRemote {
                background_id: 3_000_000_999,
            });
        let (promise, slot) = capture();
        manager.set_background(
            &mut env,
            Some(&remote),
            Some(&wallpaper_type()),
            false,
            promise,
        );
        assert_eq!(captured_err(&slot).message(), "Background to set not found");

        // known remote background parks the install query
        manager
            .on_get_background(
                &mut env,
                BackgroundId::new(0),
                "",
                Some(wallpaper(3_000_000_100, 5, false)),
                false,
                false,
                false,
            )
            .unwrap();
        let remote =
            td_api::InputBackground::inputBackgroundRemote(td_api::inputBackgroundRemote {
                background_id: 3_000_000_100,
            });
        let (promise, _slot) = capture();
        manager.set_background(
            &mut env,
            Some(&remote),
            Some(&wallpaper_type()),
            false,
            promise,
        );

        // the install answer flips the state
        let (promise, slot) = capture();
        manager.on_installed_background(
            &mut env,
            BackgroundId::new(3_000_000_100),
            BackgroundType::wallpaper(false, false, 0),
            false,
            StatusResult::Ok(()),
            promise,
        );
        let background = captured_ok(&slot).unwrap();
        assert_eq!(background.id, 3_000_000_100);
        assert_eq!(env.updates.len(), 2);
    }

    #[test]
    fn delete_and_reset_backgrounds() {
        let mut env = TestEnv::new();
        let mut manager = BackgroundManager::new();
        manager.init(&mut env);

        // the start-up sent the two empty updates
        assert_eq!(env.updates.len(), 2);

        // select a local fill background first
        let (promise, slot) = capture();
        manager.set_background(&mut env, None, Some(&fill_type(0x112233)), false, promise);
        let background = captured_ok(&slot).unwrap();
        assert_eq!(env.updates.len(), 3);

        // delete clears the selection
        let (promise, slot) = capture();
        manager.delete_background(&mut env, false, promise);
        assert!(captured(&slot).unwrap().is_ok());
        assert_eq!(env.updates.len(), 4);
        assert!(matches!(
            &env.updates[3],
            td_api::Update::updateDefaultBackground(td_api::updateDefaultBackground {
                for_dark_theme: false,
                background: None,
            })
        ));

        // remove an unknown background errors
        let (promise, slot) = capture();
        manager.remove_background(&mut env, BackgroundId::new(3_000_000_999), promise);
        assert_eq!(captured_err(&slot).code(), 400);

        // remove a local fill background resolves immediately
        let (promise, slot) = capture();
        manager.remove_background(&mut env, BackgroundId::new(background.id), promise);
        assert!(captured(&slot).unwrap().is_ok());

        // reset parks the query
        let (promise, _slot) = capture();
        manager.reset_backgrounds(&mut env, promise);

        // the reset answer clears the state
        let (promise, slot) = capture();
        manager.on_reset_background(&mut env, StatusResult::Ok(()), promise);
        assert!(captured(&slot).unwrap().is_ok());
    }

    #[test]
    fn get_backgrounds_parks_and_answers() {
        let mut env = TestEnv::new();
        let mut manager = BackgroundManager::new();

        let (promise, slot) = capture();
        manager.get_backgrounds(&mut env, false, promise);
        let (second_promise, second_slot) = capture();
        manager.get_backgrounds(&mut env, true, second_promise);

        // the error answers all the parked promises
        manager.on_get_backgrounds(
            &mut env,
            StatusResult::from_status(Status::error(500, "Flood wait")),
        );
        assert_eq!(captured_err(&slot).code(), 500);
        assert_eq!(captured_err(&second_slot).code(), 500);

        // the not-modified answer
        let (promise, slot) = capture();
        manager.get_backgrounds(&mut env, false, promise);
        manager.on_get_backgrounds(
            &mut env,
            StatusResult::Ok(
                telegram_api::account_WallPapers::account_wallPapersNotModified(
                    telegram_api::account_wallPapersNotModified,
                ),
            ),
        );
        assert_eq!(captured_ok(&slot).backgrounds.len(), 0);

        // the wallpapers answer
        let (promise, slot) = capture();
        manager.get_backgrounds(&mut env, false, promise);
        manager.on_get_backgrounds(
            &mut env,
            StatusResult::Ok(telegram_api::account_WallPapers::account_wallPapers(
                telegram_api::account_wallPapers {
                    hash: 1,
                    wallpapers: vec![
                        wallpaper(3_000_000_001, 10, false),
                        wallpaper(3_000_000_002, 11, true),
                    ],
                },
            )),
        );
        let backgrounds = captured_ok(&slot);
        assert_eq!(backgrounds.backgrounds.len(), 2);
        assert_eq!(backgrounds.backgrounds[0].id, 3_000_000_001);
        assert_eq!(backgrounds.backgrounds[1].id, 3_000_000_002);
    }

    #[test]
    fn get_backgrounds_object_sorts_selected_and_local_first() {
        let mut env = TestEnv::new();
        let mut manager = BackgroundManager::new();
        manager.init(&mut env);

        // installed wallpapers
        for (id, document_id) in [
            (3_000_000_001, 10),
            (3_000_000_002, 11),
            (3_000_000_003, 12),
        ] {
            manager
                .on_get_background(
                    &mut env,
                    BackgroundId::new(0),
                    "",
                    Some(wallpaper(id, document_id, false)),
                    false,
                    false,
                    false,
                )
                .unwrap();
        }
        // the C++ on_get_backgrounds answer pushes every received wallpaper
        // into installed_backgrounds
        manager.installed_backgrounds.push((
            BackgroundId::new(3_000_000_002),
            BackgroundType::wallpaper(false, false, 0),
        ));
        manager.installed_backgrounds.push((
            BackgroundId::new(3_000_000_003),
            BackgroundType::wallpaper(false, false, 0),
        ));
        // a local fill background (the set_background local path pushes it
        // into local_background_ids and selects it)
        let (promise, slot) = capture();
        manager.set_background(&mut env, None, Some(&fill_type(0xFFFFFF)), false, promise);
        let local_id = background_id_of(&captured_ok(&slot));
        // install the first wallpaper — the selection flips to it
        let (promise, _slot) = capture();
        manager.on_installed_background(
            &mut env,
            BackgroundId::new(3_000_000_001),
            BackgroundType::wallpaper(false, false, 0),
            false,
            StatusResult::Ok(()),
            promise,
        );

        let backgrounds = manager.get_backgrounds_object(&mut env, false);
        assert_eq!(backgrounds.backgrounds.len(), 4);
        // the selected background is first
        assert_eq!(backgrounds.backgrounds[0].id, 3_000_000_001);
        // the local background is next (local_score 0, theme_score 0)
        assert_eq!(backgrounds.backgrounds[1].id, local_id);
        // the remote wallpapers follow in order
        assert_eq!(backgrounds.backgrounds[2].id, 3_000_000_002);
        assert_eq!(backgrounds.backgrounds[3].id, 3_000_000_003);
    }

    #[test]
    fn set_dialog_background_paths() {
        let mut env = TestEnv::new();
        let mut manager = BackgroundManager::new();

        let remote =
            td_api::InputBackground::inputBackgroundRemote(td_api::inputBackgroundRemote {
                background_id: 3_000_000_100,
            });

        // unknown dialog
        let (promise, slot) = capture();
        manager.set_dialog_background(
            &mut env,
            DialogId::new(0),
            Some(&remote),
            Some(&wallpaper_type()),
            50,
            false,
            promise,
        );
        assert_eq!(captured_err(&slot).code(), 400);

        // unknown background
        let (promise, slot) = capture();
        manager.set_dialog_background(
            &mut env,
            DialogId::new(1),
            Some(&remote),
            Some(&wallpaper_type()),
            50,
            false,
            promise,
        );
        assert_eq!(captured_err(&slot).message(), "Background to set not found");

        // known background parks the chat query
        manager
            .on_get_background(
                &mut env,
                BackgroundId::new(0),
                "",
                Some(wallpaper(3_000_000_100, 5, false)),
                false,
                false,
                false,
            )
            .unwrap();
        let (promise, _slot) = capture();
        manager.set_dialog_background(
            &mut env,
            DialogId::new(1),
            Some(&remote),
            Some(&wallpaper_type()),
            50,
            false,
            promise,
        );

        // delete parks the chat query too
        let (promise, _slot) = capture();
        manager.delete_dialog_background(&mut env, DialogId::new(1), true, promise);

        // the previous-background message must be a server message
        let previous =
            td_api::InputBackground::inputBackgroundPrevious(td_api::inputBackgroundPrevious {
                message_id: 0,
            });
        let (promise, slot) = capture();
        manager.set_dialog_background(
            &mut env,
            DialogId::new(1),
            Some(&previous),
            None,
            0,
            false,
            promise,
        );
        assert_eq!(
            captured_err(&slot).message(),
            "Invalid message identifier specified"
        );
    }

    #[test]
    fn uploaded_background_registers_and_resolves() {
        let mut env = TestEnv::new();
        let mut manager = BackgroundManager::new();

        // the local input file uploads (the test file manager answers the
        // uploadable file)
        env.file_views.insert(
            FileId::new(1, 0),
            FileViewInfo {
                has_full_local_location: true,
                main_file_id: FileId::new(1, 0),
                ..FileViewInfo::default()
            },
        );
        let (promise, slot) = capture();
        manager.set_background(
            &mut env,
            Some(&local_file_input()),
            Some(&wallpaper_type()),
            false,
            promise,
        );
        assert!(captured(&slot).is_none()); // parked at the upload

        let upload_id = FileUploadId::new(FileId::new(1, 0), 1);
        assert_eq!(env.uploaded, vec![upload_id]);

        // the upload callback answers ok without an input file — the
        // reupload tail finds no registered background and fails
        manager.on_upload_background_file(&mut env, upload_id, None);
        assert_eq!(
            captured_err(&slot).message(),
            "Failed to reupload background"
        );

        // a second upload whose answer registers the wallpaper resolves
        let (promise, slot) = capture();
        manager.set_background(
            &mut env,
            Some(&local_file_input()),
            Some(&wallpaper_type()),
            false,
            promise,
        );
        let upload_id = FileUploadId::new(FileId::new(1, 0), 2);
        manager.on_upload_background_file(
            &mut env,
            upload_id,
            Some(telegram_api::InputFile::inputFile(
                telegram_api::inputFile {
                    id: 3,
                    parts: 1,
                    name: String::new(),
                    md5_checksum: String::new(),
                },
            )),
        );
        // the query answer registers the wallpaper + resolves
        manager.on_uploaded_background_file(
            &mut env,
            upload_id,
            BackgroundType::wallpaper(false, false, 0),
            DialogId::new(0),
            false,
            wallpaper(3_000_000_200, 60, false),
        );
        let background = captured_ok(&slot).unwrap();
        assert_eq!(background.id, 3_000_000_200);
        // the merged upload was canceled
        assert_eq!(env.uploaded[2], upload_id);
    }

    #[test]
    fn upload_background_query_error_fails_the_continuation() {
        let mut env = TestEnv::new();
        let mut manager = BackgroundManager::new();

        // park the upload continuation (the upload callback answered with
        // a resolved input file — the query is in flight)
        env.file_views.insert(
            FileId::new(1, 0),
            FileViewInfo {
                has_full_local_location: true,
                main_file_id: FileId::new(1, 0),
                ..FileViewInfo::default()
            },
        );
        let (promise, slot) = capture();
        manager.set_background(
            &mut env,
            Some(&local_file_input()),
            Some(&wallpaper_type()),
            false,
            promise,
        );
        assert!(captured(&slot).is_none());
        let upload_id = FileUploadId::new(FileId::new(1, 0), 1);
        manager.on_upload_background_file(
            &mut env,
            upload_id,
            Some(telegram_api::InputFile::inputFile(
                telegram_api::inputFile {
                    id: 3,
                    parts: 1,
                    name: String::new(),
                    md5_checksum: String::new(),
                },
            )),
        );
        assert!(captured(&slot).is_none()); // parked at the wire query

        // the plain error band — the partial location cleans up, the
        // upload cancels, the continuation fails (cpp:231-242)
        manager.on_upload_background_query_error(
            &mut env,
            upload_id,
            Status::error(400, "WALLPAPER_INVALID"),
        );
        assert_eq!(env.deleted_partial, vec![upload_id]);
        assert_eq!(env.uploaded.last(), Some(&upload_id));
        assert_eq!(captured_err(&slot).message(), "WALLPAPER_INVALID");

        // the FILE_PART_MISSING band skips the partial-location cleanup
        // (the C++ TODO branch)
        env.deleted_partial.clear();
        manager.on_upload_background_query_error(
            &mut env,
            upload_id,
            Status::error(400, "FILE_PART_1_MISSING"),
        );
        assert!(env.deleted_partial.is_empty());
        // no continuation left — the second error is a no-op past the
        // upload cancel
        assert_eq!(env.uploaded.len(), 3);
    }

    #[test]
    fn init_loads_persisted_state() {
        let mut env = TestEnv::new();
        let mut manager = BackgroundManager::new();
        manager.init(&mut env);
        assert_eq!(env.updates.len(), 2);

        // set a local fill background
        let (promise, slot) = capture();
        manager.set_background(&mut env, None, Some(&fill_type(0x123456)), false, promise);
        let background_id = BackgroundId::new(background_id_of(&captured_ok(&slot)));

        // a second manager loads the persisted state
        let mut env2 = TestEnv::new();
        env2.pmc = env.pmc.clone();
        let mut manager2 = BackgroundManager::new();
        manager2.init(&mut env2);
        assert_eq!(env2.updates.len(), 2);
        let background = manager2
            .get_background_object(&mut env2, background_id, false, None)
            .unwrap();
        assert_eq!(background.id, background_id.get());
        // the restored update carries the background
        assert!(matches!(
            &env2.updates[0],
            td_api::Update::updateDefaultBackground(td_api::updateDefaultBackground {
                for_dark_theme: false,
                background: Some(_),
            })
        ));

        // bots load nothing
        let mut bot_env = TestEnv::new();
        bot_env.is_bot = true;
        let mut bot_manager = BackgroundManager::new();
        bot_manager.init(&mut bot_env);
        assert!(bot_env.updates.is_empty());
        let mut updates = Vec::new();
        bot_manager.get_current_state(&mut bot_env, &mut updates);
        assert!(updates.is_empty());
    }

    #[test]
    fn get_current_state_answers_two_updates() {
        let mut env = TestEnv::new();
        let mut manager = BackgroundManager::new();
        let mut updates = Vec::new();
        manager.get_current_state(&mut env, &mut updates);
        assert_eq!(updates.len(), 2);
        assert!(matches!(
            &updates[0],
            td_api::Update::updateDefaultBackground(td_api::updateDefaultBackground {
                for_dark_theme: false,
                ..
            })
        ));
        assert!(matches!(
            &updates[1],
            td_api::Update::updateDefaultBackground(td_api::updateDefaultBackground {
                for_dark_theme: true,
                ..
            })
        ));
    }

    #[test]
    fn store_background_rows_for_the_theme_seam() {
        let mut env = TestEnv::new();
        let mut manager = BackgroundManager::new();
        manager
            .on_get_background(
                &mut env,
                BackgroundId::new(0),
                "",
                Some(wallpaper(3_000_000_100, 5, false)),
                false,
                false,
                false,
            )
            .unwrap();

        let mut out = Vec::new();
        assert!(manager.store_background(&mut env, BackgroundId::new(3_000_000_100), &mut out));
        assert!(!out.is_empty());
        assert!(!manager.store_background(&mut env, BackgroundId::new(3_000_000_999), &mut out));

        let mut parser = Parser::new(&out);
        let parsed = manager.parse_background(&mut env, &mut parser).unwrap();
        assert_eq!(parsed, BackgroundId::new(3_000_000_100));
    }
}
