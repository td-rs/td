//! `ThemeManager` (td/telegram/ThemeManager.{h,cpp}) — the M.41 Wave-3
//! slice: the emoji chat themes, the chat/profile accent colors and the
//! unique-gift chat themes, together with the value plane the slice owns
//! per the Wave-3 ledger: `ThemeSettings` + `BaseTheme` + `ChatTheme`
//! (ThemeManager.cpp/ChatTheme.cpp/ThemeSettings.cpp/BaseTheme.cpp) and
//! the `BackgroundType`/`BackgroundFill`/`BackgroundInfo` byte plane those
//! persist.
//!
//! Seams:
//! - The four `Td::ResultHandler` queries (`GetChatThemesQuery`,
//!   `GetPeerColorsQuery`, `GetPeerProfileColorsQuery`,
//!   `GetUniqueGiftChatThemesQuery`) ride the client-side dispatcher
//!   under their `ThemeNetKind` — the answers drain through
//!   `Td::on_theme_net_answer` into the three real, unit-tested intake
//!   handlers and the parked get-unique-gift promises.
//! - `ThemeSettings`' wallpaper rides
//!   [`ThemeManagerEnv::on_get_theme_background`] — the
//!   `BackgroundManager::on_get_background` collapse (BackgroundManager
//!   is its own track): the wallPaper half resolves the id inline and
//!   registers the document into the M.12 file plane; the
//!   `wallPaperNoFile` local-id counter (`set_local_background_id`) is
//!   [blocked: M.16 Backgrounds] and reports an unresolvable row.
//! - The `BackgroundId` row bytes inside `BackgroundInfo::store/parse`
//!   ride [`ThemeManagerEnv::store_background_row`]/[`ThemeManagerEnv::parse_background_row`]
//!   (`BackgroundManager::store_background`/`parse_background`) — the full
//!   `Background` row store is [blocked: M.16]; until then a theme whose
//!   wallpaper row is unresolvable stores as background-less (the
//!   `has_background` flag stays consistent through the same env).
//! - `ChatTheme`'s Gift variant rides
//!   [`ThemeManagerEnv::star_gift_object_for_theme`] — the
//!   `StarGift`/`get_upgraded_gift_object` collapse ([blocked: M.42
//!   StarGift]; the M.9 4-variant deferral unblocks there too).
//! - `UpdatesManager::on_update(updateTheme)` →
//!   [`ThemeManager::on_update_theme`] and the after-getDifference reload
//!   block (UpdatesManager.cpp:2459-2461 →
//!   [`ThemeManager::reload_accent_colors`]/[`ThemeManager::reload_chat_themes`]/[
//!   `ThemeManager::reload_profile_accent_colors`]) are the M.7 seams,
//!   wired at the updates integration.
//! - `BoostManager::get_theme_counts` (BoostManager.cpp:374) and the
//!   ChatManager/DialogEventLog accent-color object conversions
//!   (ChatManager.cpp:2943/2966/2998) call the public accessors through
//!   their own tracks' integration.
//!
//! Recorded divergences:
//! - `ThemeManager::get_current_state` is implemented (and unit-tested)
//!   but Requests.cpp (1.8.67) never calls it — the manager is absent from
//!   the `Requests::get_current_state` block, so the updateEmojiChatThemes/
//!   updateAccentColors/updateProfileAccentColors re-sends arrive only
//!   through the load/reload paths. The Rust mirror keeps the same shape.
//! - The C++ `AccentColors`/`ProfileAccentColors` stores iterate
//!   `td::FlatHashMap`, whose iteration order is randomized per iteration
//!   (FlatHashTable.h:441 `get_random_flat_hash_table_bucket`), so the C++
//!   pmc bytes are not reproducible to begin with; the Rust port pins the
//!   deterministic insertion order instead (the load side is
//!   order-insensitive both ways).

use crate::peer_color::AccentColorId;
use crate::store_parse::{self, Parser};
use crate::td_api;
use std::collections::HashMap;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// `td::BackgroundId` (BackgroundId.h) — the canonical id wrapper lives in
/// [`crate::background_manager`] (the Backgrounds slice); the theme plane
/// re-exports it.
pub use crate::background_manager::BackgroundId;

/// `td::BaseTheme` (BaseTheme.h) — append-only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BaseTheme {
    #[default]
    Classic = 0,
    Day = 1,
    Night = 2,
    Tinted = 3,
    Arctic = 4,
}

/// `is_dark_base_theme` (BaseTheme.cpp:17-30).
pub fn is_dark_base_theme(base_theme: BaseTheme) -> bool {
    matches!(base_theme, BaseTheme::Night | BaseTheme::Tinted)
}

/// `get_base_theme` (BaseTheme.cpp:32-48).
pub fn get_base_theme(base_theme: &telegram_api::BaseTheme) -> BaseTheme {
    match base_theme {
        telegram_api::BaseTheme::baseThemeClassic(_) => BaseTheme::Classic,
        telegram_api::BaseTheme::baseThemeDay(_) => BaseTheme::Day,
        telegram_api::BaseTheme::baseThemeNight(_) => BaseTheme::Night,
        telegram_api::BaseTheme::baseThemeTinted(_) => BaseTheme::Tinted,
        telegram_api::BaseTheme::baseThemeArctic(_) => BaseTheme::Arctic,
    }
}

/// `get_built_in_theme_object` (BaseTheme.cpp:50-68).
pub fn get_built_in_theme_object(base_theme: BaseTheme) -> td_api::BuiltInTheme {
    match base_theme {
        BaseTheme::Classic => {
            td_api::BuiltInTheme::builtInThemeClassic(td_api::builtInThemeClassic)
        }
        BaseTheme::Day => td_api::BuiltInTheme::builtInThemeDay(td_api::builtInThemeDay),
        BaseTheme::Night => td_api::BuiltInTheme::builtInThemeNight(td_api::builtInThemeNight),
        BaseTheme::Tinted => td_api::BuiltInTheme::builtInThemeTinted(td_api::builtInThemeTinted),
        BaseTheme::Arctic => td_api::BuiltInTheme::builtInThemeArctic(td_api::builtInThemeArctic),
    }
}

/// `validate_alpha_color` (BackgroundType.cpp:17-24): masks to 24 bits and
/// reports validity; an out-of-range color is zeroed like the C++.
fn validate_alpha_color(color: &mut i32) -> bool {
    if (-0x100_0000..=0xFF_FFFF).contains(color) {
        *color &= 0xFF_FFFF;
        return true;
    }
    *color = 0;
    false
}

/// `is_valid_rotation_angle` (BackgroundType.cpp:26-28).
fn is_valid_rotation_angle(rotation_angle: i32) -> bool {
    (0..360).contains(&rotation_angle) && rotation_angle % 45 == 0
}

/// `is_valid_intensity` (BackgroundType.cpp:30-32).
fn is_valid_intensity(intensity: i32, allow_negative: bool) -> bool {
    (if allow_negative { -100 } else { 0 }..=100).contains(&intensity)
}

/// `td::BackgroundFill` (BackgroundType.h:17-77) — the value plane the
/// theme settings persist; the `get_link`/`is_dark`/td_api-input halves
/// are the Backgrounds-track slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BackgroundFill {
    pub(crate) top_color: i32,
    pub(crate) bottom_color: i32,
    pub(crate) rotation_angle: i32,
    pub(crate) third_color: i32,
    pub(crate) fourth_color: i32,
}

impl Default for BackgroundFill {
    /// The C++ member initializers (BackgroundType.h:20-24): the freeform
    /// colors default to the -1 sentinel so `get_type` never misreads a
    /// zero-initialized fill as FreeformGradient.
    fn default() -> Self {
        BackgroundFill {
            top_color: 0,
            bottom_color: 0,
            rotation_angle: 0,
            third_color: -1,
            fourth_color: -1,
        }
    }
}

/// The `BackgroundFill::Type` discriminator (`get_type`, BackgroundType.h:50-58).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum BackgroundFillType {
    #[default]
    Solid,
    Gradient,
    FreeformGradient,
}

impl BackgroundFill {
    /// `BackgroundFill::Type get_type` — derived from the stored colors.
    pub fn get_type(&self) -> BackgroundFillType {
        if self.third_color != -1 {
            return BackgroundFillType::FreeformGradient;
        }
        if self.top_color == self.bottom_color {
            return BackgroundFillType::Solid;
        }
        BackgroundFillType::Gradient
    }

    /// `BackgroundFill::BackgroundFill(const telegram_api::wallPaperSettings *)`
    /// (BackgroundType.cpp:99-164).
    pub fn from_wall_paper_settings(
        settings: Option<&telegram_api::WallPaperSettings>,
    ) -> BackgroundFill {
        let mut fill = BackgroundFill::default();
        let Some(settings) = settings else {
            return fill;
        };

        let flags = settings.flags;
        if !settings.emoticon.is_empty() {
            tdutils::log_error!("Receive filled background with an emoticon");
        }
        if flags & 1 != 0 {
            // BACKGROUND_COLOR_MASK
            fill.top_color = settings.background_color;
            if !validate_alpha_color(&mut fill.top_color) {
                tdutils::log_error!("Receive invalid background color");
            }
        }
        if flags & (1 << 6) != 0 || flags & (1 << 5) != 0 {
            // FOURTH_/THIRD_BACKGROUND_COLOR_MASK
            fill.bottom_color = settings.second_background_color;
            if !validate_alpha_color(&mut fill.bottom_color) {
                tdutils::log_error!("Receive invalid second background color");
            }
            fill.third_color = settings.third_background_color;
            if !validate_alpha_color(&mut fill.third_color) {
                tdutils::log_error!("Receive invalid third background color");
            }
            if flags & (1 << 6) != 0 {
                fill.fourth_color = settings.fourth_background_color;
                if !validate_alpha_color(&mut fill.fourth_color) {
                    tdutils::log_error!("Receive invalid fourth background color");
                }
            }
        } else if flags & (1 << 4) != 0 {
            // SECOND_BACKGROUND_COLOR_MASK
            fill.bottom_color = settings.second_background_color;
            if !validate_alpha_color(&mut fill.bottom_color) {
                tdutils::log_error!("Receive invalid second background color");
            }
            fill.rotation_angle = settings.rotation;
            if !is_valid_rotation_angle(fill.rotation_angle) {
                tdutils::log_error!("Receive invalid rotation angle");
                fill.rotation_angle = 0;
            }
        } else {
            fill.bottom_color = fill.top_color;
        }
        if fill.get_type() != BackgroundFillType::Gradient {
            fill.rotation_angle = 0;
        }
        fill
    }

    /// `BackgroundFill::get_background_fill_object` (BackgroundType.cpp:434-451).
    pub fn get_background_fill_object(&self) -> td_api::BackgroundFill {
        match self.get_type() {
            BackgroundFillType::Solid => {
                td_api::BackgroundFill::backgroundFillSolid(td_api::backgroundFillSolid {
                    color: self.top_color,
                })
            }
            BackgroundFillType::Gradient => {
                td_api::BackgroundFill::backgroundFillGradient(td_api::backgroundFillGradient {
                    top_color: self.top_color,
                    bottom_color: self.bottom_color,
                    rotation_angle: self.rotation_angle,
                })
            }
            BackgroundFillType::FreeformGradient => {
                let mut colors = vec![
                    self.top_color,
                    self.bottom_color,
                    self.third_color,
                    self.fourth_color,
                ];
                if colors[3] == -1 {
                    colors.pop();
                }
                td_api::BackgroundFill::backgroundFillFreeformGradient(
                    td_api::backgroundFillFreeformGradient { colors },
                )
            }
        }
    }

    /// `BackgroundFill::get_background_fill_object(const vector<int32> &)`
    /// (BackgroundType.cpp:453-464) — the message-colors conversion.
    pub fn get_background_fill_object_from_colors(
        colors: &[i32],
    ) -> Option<td_api::BackgroundFill> {
        if colors.len() >= 3 {
            return Some(td_api::BackgroundFill::backgroundFillFreeformGradient(
                td_api::backgroundFillFreeformGradient {
                    colors: colors.to_vec(),
                },
            ));
        }
        let first = *colors.first()?;
        if colors.len() == 1 || colors[0] == colors[1] {
            return Some(td_api::BackgroundFill::backgroundFillSolid(
                td_api::backgroundFillSolid { color: first },
            ));
        }
        Some(td_api::BackgroundFill::backgroundFillGradient(
            td_api::backgroundFillGradient {
                top_color: colors[1],
                bottom_color: colors[0],
                rotation_angle: 0,
            },
        ))
    }
}

/// `td::BackgroundType` (BackgroundType.h:79-162) — the value plane the
/// theme settings persist; the link/input/local-name halves are the
/// Backgrounds-track slice.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct BackgroundType {
    pub(crate) r#type: BackgroundTypeType,
    pub(crate) is_blurred: bool,
    pub(crate) is_moving: bool,
    pub(crate) intensity: i32,
    pub(crate) fill: BackgroundFill,
    pub(crate) theme_name: String,
}

/// The `BackgroundType::Type` discriminator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum BackgroundTypeType {
    Wallpaper = 0,
    Pattern = 1,
    #[default]
    Fill = 2,
    ChatTheme = 3,
}

impl BackgroundType {
    /// `BackgroundType::BackgroundType(bool has_no_file, bool is_pattern,
    /// wallPaperSettings)` (BackgroundType.cpp:305-341).
    pub fn from_settings(
        has_no_file: bool,
        is_pattern: bool,
        settings: Option<&telegram_api::WallPaperSettings>,
    ) -> BackgroundType {
        let mut result = BackgroundType::default();
        if let Some(settings) = settings
            && settings.flags & (1 << 3) != 0
        {
            // INTENSITY_MASK
            result.intensity = settings.intensity;
            if !is_valid_intensity(result.intensity, is_pattern) {
                tdutils::log_error!("Receive invalid intensity");
                result.intensity = if is_pattern { 50 } else { 0 };
            }
        }
        if has_no_file {
            let Some(settings) = settings else {
                // the C++ CHECK(settings != nullptr)
                unreachable!("has_no_file background type needs settings");
            };
            if !settings.emoticon.is_empty() {
                result.r#type = BackgroundTypeType::ChatTheme;
                result.theme_name = settings.emoticon.clone();
            } else {
                result.r#type = BackgroundTypeType::Fill;
                result.fill = BackgroundFill::from_wall_paper_settings(Some(settings));
            }
        } else if is_pattern {
            result.r#type = BackgroundTypeType::Pattern;
            if let Some(settings) = settings {
                result.fill = BackgroundFill::from_wall_paper_settings(Some(settings));
                result.is_moving = settings.motion;
            }
        } else {
            result.r#type = BackgroundTypeType::Wallpaper;
            if let Some(settings) = settings {
                result.is_blurred = settings.blur;
                result.is_moving = settings.motion;
            }
        }
        result
    }

    /// `BackgroundType::has_file` (BackgroundType.h:114-116).
    pub fn has_file(&self) -> bool {
        matches!(
            self.r#type,
            BackgroundTypeType::Wallpaper | BackgroundTypeType::Pattern
        )
    }

    /// `BackgroundType::get_dark_theme_dimming` (BackgroundType.h:136-141).
    pub fn get_dark_theme_dimming(&self) -> i32 {
        if self.r#type == BackgroundTypeType::Pattern {
            return 0;
        }
        self.intensity
    }

    /// `BackgroundType::get_background_type_object` (BackgroundType.cpp:466-481).
    pub fn get_background_type_object(&self) -> td_api::BackgroundType {
        match self.r#type {
            BackgroundTypeType::Wallpaper => {
                td_api::BackgroundType::backgroundTypeWallpaper(td_api::backgroundTypeWallpaper {
                    is_blurred: self.is_blurred,
                    is_moving: self.is_moving,
                })
            }
            BackgroundTypeType::Pattern => {
                td_api::BackgroundType::backgroundTypePattern(td_api::backgroundTypePattern {
                    fill: Some(self.fill.get_background_fill_object()),
                    intensity: if self.intensity < 0 {
                        -self.intensity
                    } else {
                        self.intensity
                    },
                    is_inverted: self.intensity < 0,
                    is_moving: self.is_moving,
                })
            }
            BackgroundTypeType::Fill => {
                td_api::BackgroundType::backgroundTypeFill(td_api::backgroundTypeFill {
                    fill: Some(self.fill.get_background_fill_object()),
                })
            }
            BackgroundTypeType::ChatTheme => {
                td_api::BackgroundType::backgroundTypeChatTheme(td_api::backgroundTypeChatTheme {
                    theme_name: self.theme_name.clone(),
                })
            }
        }
    }

    /// `BackgroundType::store` (BackgroundType.hpp:18-56) — byte-exact.
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_fill = self.fill.top_color != 0 || self.fill.bottom_color != 0;
        let has_intensity = self.intensity != 0;
        let fill_type = self.fill.get_type();
        let is_gradient = fill_type == BackgroundFillType::Gradient;
        let is_freeform_gradient = fill_type == BackgroundFillType::FreeformGradient;
        let has_theme_name = !self.theme_name.is_empty();
        store_parse::store_flags(
            out,
            [
                self.is_blurred,
                self.is_moving,
                has_fill,
                has_intensity,
                is_gradient,
                is_freeform_gradient,
                has_theme_name,
            ],
        );
        store_parse::store_i32(out, self.r#type as i32);
        if is_freeform_gradient {
            store_parse::store_i32(out, self.fill.top_color);
            store_parse::store_i32(out, self.fill.bottom_color);
            store_parse::store_i32(out, self.fill.third_color);
            store_parse::store_i32(out, self.fill.fourth_color);
        } else if has_fill {
            store_parse::store_i32(out, self.fill.top_color);
            if is_gradient {
                store_parse::store_i32(out, self.fill.bottom_color);
                store_parse::store_i32(out, self.fill.rotation_angle);
            }
        }
        if has_intensity {
            store_parse::store_i32(out, self.intensity);
        }
        if has_theme_name {
            store_parse::store_string(out, &self.theme_name);
        }
    }

    /// `BackgroundType::parse` (BackgroundType.hpp:58-92) — byte-exact.
    pub fn parse(parser: &mut Parser) -> Result<BackgroundType, &'static str> {
        let mut result = BackgroundType::default();
        let [
            is_blurred,
            is_moving,
            has_fill,
            has_intensity,
            is_gradient,
            is_freeform_gradient,
            has_theme_name,
        ] = parser.parse_flags()?;
        result.is_blurred = is_blurred;
        result.is_moving = is_moving;
        result.r#type = match parser.i32_()? {
            0 => BackgroundTypeType::Wallpaper,
            1 => BackgroundTypeType::Pattern,
            2 => BackgroundTypeType::Fill,
            3 => BackgroundTypeType::ChatTheme,
            _ => return Err("Invalid background type"),
        };
        if is_freeform_gradient {
            result.fill.top_color = parser.i32_()?;
            result.fill.bottom_color = parser.i32_()?;
            result.fill.third_color = parser.i32_()?;
            result.fill.fourth_color = parser.i32_()?;
        } else if has_fill {
            result.fill.top_color = parser.i32_()?;
            if is_gradient {
                result.fill.bottom_color = parser.i32_()?;
                result.fill.rotation_angle = parser.i32_()?;
            } else {
                result.fill.bottom_color = result.fill.top_color;
            }
        }
        if has_intensity {
            result.intensity = parser.i32_()?;
        }
        if has_theme_name {
            result.theme_name = parser.string()?.to_owned();
        }
        Result::Ok(result)
    }
}

/// `td::BackgroundInfo` (BackgroundInfo.h) — the wallpaper reference the
/// theme settings persist. The ctor's `BackgroundManager::on_get_background`
/// collapse and the id-row store/parse ride the env; `get_chat_background_object`
/// is the Backgrounds-track slice.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BackgroundInfo {
    pub(crate) background_id: BackgroundId,
    pub(crate) background_type: BackgroundType,
}

impl BackgroundInfo {
    /// `BackgroundInfo::BackgroundInfo(Td *, WallPaper, allow_empty,
    /// is_no_file_pattern)` (BackgroundInfo.cpp:18-27) through the
    /// `on_get_background` seam; `None` models the C++ empty return.
    pub fn from_telegram(
        env: &mut impl ThemeManagerEnv,
        wallpaper: Option<telegram_api::WallPaper>,
        is_no_file_pattern: bool,
    ) -> Option<BackgroundInfo> {
        let (background_id, background_type) =
            env.on_get_theme_background(wallpaper, is_no_file_pattern)?;
        Some(BackgroundInfo {
            background_id,
            background_type,
        })
    }

    /// `BackgroundInfo::is_valid` — a valid background id.
    pub fn is_valid(&self) -> bool {
        self.background_id.is_valid()
    }

    /// `BackgroundInfo::get_background_object` (BackgroundInfo.cpp:29-33)
    /// through the env; `None` is the [blocked: M.16] background cache.
    pub fn get_background_object(
        &self,
        env: &mut impl ThemeManagerEnv,
    ) -> Option<td_api::Background> {
        env.get_theme_background_object(self)
    }

    /// `BackgroundInfo::store` (BackgroundInfo.hpp:18-27) — the id row
    /// through the env seam, then the type bytes.
    pub fn store(&self, env: &mut impl ThemeManagerEnv, out: &mut Vec<u8>) -> bool {
        if !env.store_background_row(self.background_id, out) {
            return false;
        }
        self.background_type.store(out);
        true
    }

    /// `BackgroundInfo::parse` (BackgroundInfo.hpp:29-36).
    pub fn parse(
        env: &mut impl ThemeManagerEnv,
        parser: &mut Parser,
    ) -> Result<BackgroundInfo, &'static str> {
        let background_id = env.parse_background_row(parser)?;
        let background_type = BackgroundType::parse(parser)?;
        Result::Ok(BackgroundInfo {
            background_id,
            background_type,
        })
    }
}

/// `td::ChatTheme` (ChatTheme.h) — the dialog-theme value class. The Gift
/// variant's `StarGift` conversion rides the env ([blocked: M.42]); the
/// store/parse (ChatTheme.hpp) persist dialog themes and land with the
/// messages seam, not this slice.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChatTheme {
    r#type: ChatThemeType,
    emoji: String,
    gift: Option<td_api::UpgradedGift>,
    light_theme: ThemeSettings,
    dark_theme: ThemeSettings,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChatThemeType {
    #[default]
    Default = 0,
    Emoji = 1,
    Gift = 2,
}

impl ChatTheme {
    /// `ChatTheme::ChatTheme(Td *, telegram_api::ChatTheme)` (ChatTheme.cpp:15-75).
    pub fn from_telegram(
        env: &mut impl ThemeManagerEnv,
        theme: telegram_api::ChatTheme,
    ) -> ChatTheme {
        match theme {
            telegram_api::ChatTheme::chatTheme(theme) => {
                if theme.emoticon.is_empty() {
                    return ChatTheme::default();
                }
                ChatTheme {
                    r#type: ChatThemeType::Emoji,
                    emoji: theme.emoticon,
                    ..ChatTheme::default()
                }
            }
            telegram_api::ChatTheme::chatThemeUniqueGift(theme) => {
                let Some(gift) = theme
                    .gift
                    .and_then(|gift| env.star_gift_object_for_theme(gift))
                else {
                    // the C++ invalid/not-unique star gift bail (cpp:32-35)
                    tdutils::log_error!("Receive chat theme with an invalid star gift");
                    return ChatTheme::default();
                };

                let mut result = ChatTheme::default();
                let mut was_light = false;
                let mut was_dark = false;
                for settings in theme.theme_settings {
                    let theme_settings = ThemeSettings::from_telegram(env, Some(settings), true);
                    if theme_settings.is_empty() {
                        tdutils::log_error!("Receive empty chat theme settings");
                        continue;
                    }
                    if theme_settings.are_dark() {
                        if !was_dark {
                            was_dark = true;
                            result.dark_theme = theme_settings;
                        } else {
                            tdutils::log_error!("Receive duplicate dark theme");
                        }
                    } else if !was_light {
                        was_light = true;
                        result.light_theme = theme_settings;
                    } else {
                        tdutils::log_error!("Receive duplicate light theme");
                    }
                }
                if result.light_theme.is_empty() || result.dark_theme.is_empty() {
                    tdutils::log_error!("Receive chat theme with invalid themes");
                    return ChatTheme::default();
                }
                result.r#type = ChatThemeType::Gift;
                result.gift = Some(gift);
                result
            }
        }
    }

    /// `ChatTheme::emoji` (ChatTheme.cpp:77-84).
    pub fn emoji(emoji: &str) -> ChatTheme {
        let mut result = ChatTheme::default();
        if !emoji.is_empty() {
            result.r#type = ChatThemeType::Emoji;
            result.emoji = emoji.to_owned();
        }
        result
    }

    /// `ChatTheme::is_default`.
    pub fn is_default(&self) -> bool {
        self.r#type == ChatThemeType::Default
    }

    /// `ChatTheme::is_gift`.
    pub fn is_gift(&self) -> bool {
        self.r#type == ChatThemeType::Gift
    }

    /// `ChatTheme::get_gift_chat_theme_object` (ChatTheme.cpp:86-91).
    pub fn get_gift_chat_theme_object(
        &self,
        env: &mut impl ThemeManagerEnv,
    ) -> td_api::GiftChatTheme {
        assert!(self.r#type == ChatThemeType::Gift);
        td_api::GiftChatTheme {
            gift: Some(self.gift.clone().expect("gift chat theme carries its gift")),
            light_settings: Some(self.light_theme.get_theme_settings_object(env)),
            dark_settings: Some(self.dark_theme.get_theme_settings_object(env)),
        }
    }

    /// `ChatTheme::get_chat_theme_object` (ChatTheme.cpp:93-104).
    pub fn get_chat_theme_object(
        &self,
        env: &mut impl ThemeManagerEnv,
    ) -> Option<td_api::ChatTheme> {
        match self.r#type {
            ChatThemeType::Default => None,
            ChatThemeType::Emoji => {
                Some(td_api::ChatTheme::chatThemeEmoji(td_api::chatThemeEmoji {
                    name: self.emoji.clone(),
                }))
            }
            ChatThemeType::Gift => Some(td_api::ChatTheme::chatThemeGift(td_api::chatThemeGift {
                gift_theme: Some(self.get_gift_chat_theme_object(env)),
            })),
        }
    }
}

/// `ThemeManager::EmojiChatTheme` (ThemeManager.h:60-71) — the private
/// emoji theme row with its byte-exact store/parse (ThemeManager.cpp:181-199).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EmojiChatTheme {
    pub emoji: String,
    pub id: i64,
    pub light_theme: ThemeSettings,
    pub dark_theme: ThemeSettings,
}

impl EmojiChatTheme {
    /// `EmojiChatTheme::store` — the empty flag word is part of the C++
    /// format (BEGIN_STORE_FLAGS with no flags); the env rides along
    /// because the inner theme settings may store the background row.
    pub fn store(&self, env: &mut impl ThemeManagerEnv, out: &mut Vec<u8>) {
        store_parse::store_flags(out, []);
        store_parse::store_string(out, &self.emoji);
        store_parse::store_i64(out, self.id);
        self.light_theme.store_inner(env, out);
        self.dark_theme.store_inner(env, out);
    }

    /// `EmojiChatTheme::parse`.
    pub fn parse(
        env: &mut impl ThemeManagerEnv,
        parser: &mut Parser,
    ) -> Result<EmojiChatTheme, &'static str> {
        parser.parse_flags::<0>()?;
        let result = EmojiChatTheme {
            emoji: parser.string()?.to_owned(),
            id: parser.i64_()?,
            light_theme: ThemeSettings::parse_inner(env, parser)?,
            dark_theme: ThemeSettings::parse_inner(env, parser)?,
        };
        Result::Ok(result)
    }
}

/// `ThemeManager::EmojiChatThemes` (ThemeManager.h:73-82) — the
/// "chat_themes" pmc row.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EmojiChatThemes {
    pub hash: i64,
    pub themes: Vec<EmojiChatTheme>,
}

impl EmojiChatThemes {
    /// `EmojiChatThemes::store` (ThemeManager.cpp:201-205).
    pub fn store(&self, env: &mut impl ThemeManagerEnv, out: &mut Vec<u8>) {
        store_parse::store_i64(out, self.hash);
        store_parse::store_i32(out, self.themes.len() as i32);
        for theme in &self.themes {
            theme.store(env, out);
        }
    }

    /// `EmojiChatThemes::parse` (ThemeManager.cpp:207-211).
    pub fn parse(
        env: &mut impl ThemeManagerEnv,
        parser: &mut Parser,
    ) -> Result<EmojiChatThemes, &'static str> {
        let hash = parser.i64_()?;
        let size = parser.i32_()?;
        let mut themes = Vec::new();
        for _ in 0..size {
            themes.push(EmojiChatTheme::parse(env, parser)?);
        }
        Result::Ok(EmojiChatThemes { hash, themes })
    }
}

/// The insertion-ordered `FlatHashMap<AccentColorId, V>` stand-in: the C++
/// iteration order is randomized (see the module divergences), so the port
/// pins insertion order; `emplace` keeps the first insert (the C++
/// `emplace` never overwrites).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccentColorMap<V> {
    entries: Vec<(AccentColorId, V)>,
}

impl<V> AccentColorMap<V> {
    pub fn new() -> AccentColorMap<V> {
        AccentColorMap {
            entries: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn contains(&self, key: AccentColorId) -> bool {
        self.entries.iter().any(|(k, _)| *k == key)
    }

    pub fn get(&self, key: AccentColorId) -> Option<&V> {
        self.entries.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: AccentColorId) -> Option<&mut V> {
        self.entries
            .iter_mut()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    }

    /// `operator[] = value` — insert-or-overwrite.
    pub fn set(&mut self, key: AccentColorId, value: V) {
        if let Some(slot) = self.get_mut(key) {
            *slot = value;
        } else {
            self.entries.push((key, value));
        }
    }

    /// `emplace(key, value)` — first insert wins.
    pub fn emplace(&mut self, key: AccentColorId, value: V) {
        if !self.contains(key) {
            self.entries.push((key, value));
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (AccentColorId, &V)> {
        self.entries.iter().map(|(k, v)| (*k, v))
    }
}

/// `ThemeManager::AccentColors` (ThemeManager.h:84-99) — the
/// "accent_colors" pmc row; `hash_` is `int32` in the C++.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccentColors {
    pub light_colors: AccentColorMap<Vec<i32>>,
    pub dark_colors: AccentColorMap<Vec<i32>>,
    pub accent_color_ids: Vec<AccentColorId>,
    pub min_broadcast_boost_levels: Vec<i32>,
    pub min_megagroup_boost_levels: Vec<i32>,
    pub hash: i32,
}

impl AccentColors {
    /// `AccentColors::store` (ThemeManager.cpp:213-243) — byte-exact with
    /// the insertion-order iteration documented in the module header.
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_hash = self.hash != 0;
        let has_min_broadcast_boost_levels = !self.min_broadcast_boost_levels.is_empty();
        let has_min_megagroup_boost_levels = !self.min_megagroup_boost_levels.is_empty();
        store_parse::store_flags(
            out,
            [
                has_hash,
                has_min_broadcast_boost_levels,
                has_min_megagroup_boost_levels,
            ],
        );
        store_parse::store_i32(out, self.light_colors.len() as i32);
        for (id, colors) in self.light_colors.iter() {
            store_parse::store_i32(out, id.get());
            store_parse::store_i32_vec(out, colors);
        }
        store_parse::store_i32(out, self.dark_colors.len() as i32);
        for (id, colors) in self.dark_colors.iter() {
            store_parse::store_i32(out, id.get());
            store_parse::store_i32_vec(out, colors);
        }
        store_parse::store_i32(out, self.accent_color_ids.len() as i32);
        for id in &self.accent_color_ids {
            store_parse::store_i32(out, id.get());
        }
        if has_hash {
            store_parse::store_i32(out, self.hash);
        }
        if has_min_broadcast_boost_levels {
            store_parse::store_i32_vec(out, &self.min_broadcast_boost_levels);
        }
        if has_min_megagroup_boost_levels {
            store_parse::store_i32_vec(out, &self.min_megagroup_boost_levels);
        }
    }

    /// `AccentColors::parse` (ThemeManager.cpp:245-286) — the C++
    /// `CHECK(accent_color_id.is_valid())` becomes the parse error the
    /// corrupt-load reset handles; the missing-min_broadcast `hash_ = 0`
    /// quirk is preserved.
    pub fn parse(parser: &mut Parser) -> Result<AccentColors, &'static str> {
        let mut result = AccentColors::default();
        let [
            has_hash,
            has_min_broadcast_boost_levels,
            has_min_megagroup_boost_levels,
        ] = parser.parse_flags()?;
        let parse_colors = |parser: &mut Parser, map: &mut AccentColorMap<Vec<i32>>| {
            let size = parser.i32_()?;
            for _ in 0..size {
                let accent_color_id = AccentColorId::new(parser.i32_()?);
                if !accent_color_id.is_valid() {
                    return Err("Invalid accent color id");
                }
                let count = parser.i32_()?;
                let mut colors = Vec::with_capacity(count.max(0) as usize);
                for _ in 0..count {
                    colors.push(parser.i32_()?);
                }
                map.emplace(accent_color_id, colors);
            }
            Result::Ok(())
        };
        parse_colors(parser, &mut result.light_colors)?;
        parse_colors(parser, &mut result.dark_colors)?;
        let ids_size = parser.i32_()?;
        for _ in 0..ids_size {
            result
                .accent_color_ids
                .push(AccentColorId::new(parser.i32_()?));
        }
        if has_hash {
            result.hash = parser.i32_()?;
        }
        if has_min_broadcast_boost_levels {
            let size = parser.i32_()?;
            for _ in 0..size {
                result.min_broadcast_boost_levels.push(parser.i32_()?);
            }
        } else {
            result.hash = 0;
        }
        if has_min_megagroup_boost_levels {
            let size = parser.i32_()?;
            for _ in 0..size {
                result.min_megagroup_boost_levels.push(parser.i32_()?);
            }
        }
        Result::Ok(result)
    }

    /// `AccentColors::get_update_accent_colors_object` (ThemeManager.cpp:677-716).
    pub fn get_update_accent_colors_object(&self) -> td_api::Update {
        let mut colors: Vec<td_api::AccentColor> = Vec::new();
        let base_colors: [i32; 7] = [
            0xDF2020, 0xDFA520, 0xA040A0, 0x208020, 0x20DFDF, 0x2044DF, 0xDF1493,
        ];
        let get_distance = |lhs_color: i32, rhs_color: i32| {
            let get_color_distance = |lhs: i32, rhs: i32| {
                let diff = (lhs & 255).max(0) - (rhs & 255).max(0);
                diff * diff
            };
            get_color_distance(lhs_color, rhs_color)
                + get_color_distance(lhs_color >> 8, rhs_color >> 8)
                + get_color_distance(lhs_color >> 16, rhs_color >> 16)
        };
        let mut min_broadcast_boost_levels: HashMap<AccentColorId, i32> = HashMap::new();
        for (i, &level) in self.min_broadcast_boost_levels.iter().enumerate() {
            assert!(i < self.accent_color_ids.len());
            min_broadcast_boost_levels.insert(self.accent_color_ids[i], level);
        }
        for (id, light_colors) in self.light_colors.iter() {
            let dark_colors = match self.dark_colors.get(id) {
                Some(dark) => dark.clone(),
                None => light_colors.clone(),
            };
            assert!(!light_colors.is_empty());
            assert!(!dark_colors.is_empty());
            let first_color = light_colors[0];
            let mut best_index = 0usize;
            let mut best_distance = get_distance(base_colors[0], first_color);
            for (i, base) in base_colors.iter().enumerate().skip(1) {
                let cur_distance = get_distance(*base, first_color);
                if cur_distance < best_distance {
                    best_distance = cur_distance;
                    best_index = i;
                }
            }
            colors.push(td_api::AccentColor {
                id: id.get(),
                built_in_accent_color_id: best_index as i32,
                light_theme_colors: light_colors.clone(),
                dark_theme_colors: dark_colors,
                min_channel_chat_boost_level: *min_broadcast_boost_levels.get(&id).unwrap_or(&0),
            });
        }
        let available_accent_color_ids: Vec<i32> =
            self.accent_color_ids.iter().map(|id| id.get()).collect();
        td_api::Update::updateAccentColors(td_api::updateAccentColors {
            colors,
            available_accent_color_ids,
        })
    }
}

/// `ThemeManager::ProfileAccentColor` (ThemeManager.h:101-115).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProfileAccentColor {
    pub palette_colors: Vec<i32>,
    pub background_colors: Vec<i32>,
    pub story_colors: Vec<i32>,
}

/// `are_colors_valid` (ThemeManager.cpp:32-42).
fn are_colors_valid(colors: &[i32], min_size: usize, max_size: usize) -> bool {
    if min_size > colors.len() || colors.len() > max_size {
        return false;
    }
    colors.iter().all(|color| (0..=0xFF_FFFF).contains(color))
}

impl ProfileAccentColor {
    /// `ProfileAccentColor::is_valid` (ThemeManager.cpp:722-725).
    pub fn is_valid(&self) -> bool {
        are_colors_valid(&self.palette_colors, 1, 2)
            && are_colors_valid(&self.background_colors, 1, 2)
            && are_colors_valid(&self.story_colors, 2, 2)
    }

    /// `ProfileAccentColor::get_profile_accent_colors_object` (ThemeManager.cpp:727-731).
    pub fn get_profile_accent_colors_object(&self) -> td_api::ProfileAccentColors {
        td_api::ProfileAccentColors {
            palette_colors: self.palette_colors.clone(),
            background_colors: self.background_colors.clone(),
            story_colors: self.story_colors.clone(),
        }
    }

    /// `ProfileAccentColor::store` (ThemeManager.cpp:288-293).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_i32_vec(out, &self.palette_colors);
        store_parse::store_i32_vec(out, &self.background_colors);
        store_parse::store_i32_vec(out, &self.story_colors);
    }

    /// `ProfileAccentColor::parse` (ThemeManager.cpp:295-300).
    pub fn parse(parser: &mut Parser) -> Result<ProfileAccentColor, &'static str> {
        let mut result = ProfileAccentColor::default();
        for target in [
            &mut result.palette_colors,
            &mut result.background_colors,
            &mut result.story_colors,
        ] {
            let size = parser.i32_()?;
            for _ in 0..size {
                target.push(parser.i32_()?);
            }
        }
        Result::Ok(result)
    }
}

/// `ThemeManager::ProfileAccentColors` (ThemeManager.h:120-135) — the
/// "profile_accent_colors" pmc row.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProfileAccentColors {
    pub light_colors: AccentColorMap<ProfileAccentColor>,
    pub dark_colors: AccentColorMap<ProfileAccentColor>,
    pub accent_color_ids: Vec<AccentColorId>,
    pub min_broadcast_boost_levels: Vec<i32>,
    pub min_megagroup_boost_levels: Vec<i32>,
    pub hash: i32,
}

impl ProfileAccentColors {
    /// `ProfileAccentColors::store` (ThemeManager.cpp:302-332).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_hash = self.hash != 0;
        let has_min_broadcast_boost_levels = !self.min_broadcast_boost_levels.is_empty();
        let has_min_megagroup_boost_levels = !self.min_megagroup_boost_levels.is_empty();
        store_parse::store_flags(
            out,
            [
                has_hash,
                has_min_broadcast_boost_levels,
                has_min_megagroup_boost_levels,
            ],
        );
        store_parse::store_i32(out, self.light_colors.len() as i32);
        for (id, colors) in self.light_colors.iter() {
            store_parse::store_i32(out, id.get());
            colors.store(out);
        }
        store_parse::store_i32(out, self.dark_colors.len() as i32);
        for (id, colors) in self.dark_colors.iter() {
            store_parse::store_i32(out, id.get());
            colors.store(out);
        }
        store_parse::store_i32(out, self.accent_color_ids.len() as i32);
        for id in &self.accent_color_ids {
            store_parse::store_i32(out, id.get());
        }
        if has_hash {
            store_parse::store_i32(out, self.hash);
        }
        if has_min_broadcast_boost_levels {
            store_parse::store_i32_vec(out, &self.min_broadcast_boost_levels);
        }
        if has_min_megagroup_boost_levels {
            store_parse::store_i32_vec(out, &self.min_megagroup_boost_levels);
        }
    }

    /// `ProfileAccentColors::parse` (ThemeManager.cpp:334-377) — both
    /// missing-boost-levels branches reset `hash_` here.
    pub fn parse(parser: &mut Parser) -> Result<ProfileAccentColors, &'static str> {
        let mut result = ProfileAccentColors::default();
        let [
            has_hash,
            has_min_broadcast_boost_levels,
            has_min_megagroup_boost_levels,
        ] = parser.parse_flags()?;
        let parse_colors = |parser: &mut Parser, map: &mut AccentColorMap<ProfileAccentColor>| {
            let size = parser.i32_()?;
            for _ in 0..size {
                let accent_color_id = AccentColorId::new(parser.i32_()?);
                if !accent_color_id.is_valid() {
                    return Err("Invalid accent color id");
                }
                map.emplace(accent_color_id, ProfileAccentColor::parse(parser)?);
            }
            Result::Ok(())
        };
        parse_colors(parser, &mut result.light_colors)?;
        parse_colors(parser, &mut result.dark_colors)?;
        let ids_size = parser.i32_()?;
        for _ in 0..ids_size {
            result
                .accent_color_ids
                .push(AccentColorId::new(parser.i32_()?));
        }
        if has_hash {
            result.hash = parser.i32_()?;
        }
        if has_min_broadcast_boost_levels {
            let size = parser.i32_()?;
            for _ in 0..size {
                result.min_broadcast_boost_levels.push(parser.i32_()?);
            }
        } else {
            result.hash = 0;
        }
        if has_min_megagroup_boost_levels {
            let size = parser.i32_()?;
            for _ in 0..size {
                result.min_megagroup_boost_levels.push(parser.i32_()?);
            }
        } else {
            result.hash = 0;
        }
        Result::Ok(result)
    }

    /// `ProfileAccentColors::get_update_profile_accent_colors_object` (ThemeManager.cpp:733-759).
    pub fn get_update_profile_accent_colors_object(&self) -> td_api::Update {
        let mut min_broadcast_boost_levels: HashMap<AccentColorId, i32> = HashMap::new();
        for (i, &level) in self.min_broadcast_boost_levels.iter().enumerate() {
            assert!(i < self.accent_color_ids.len());
            min_broadcast_boost_levels.insert(self.accent_color_ids[i], level);
        }
        let mut min_megagroup_boost_levels: HashMap<AccentColorId, i32> = HashMap::new();
        for (i, &level) in self.min_megagroup_boost_levels.iter().enumerate() {
            assert!(i < self.accent_color_ids.len());
            min_megagroup_boost_levels.insert(self.accent_color_ids[i], level);
        }
        let mut colors: Vec<td_api::ProfileAccentColor> = Vec::new();
        for (id, light) in self.light_colors.iter() {
            let dark_colors = match self.dark_colors.get(id) {
                Some(dark) => dark.get_profile_accent_colors_object(),
                None => light.get_profile_accent_colors_object(),
            };
            colors.push(td_api::ProfileAccentColor {
                id: id.get(),
                light_theme_colors: Some(light.get_profile_accent_colors_object()),
                dark_theme_colors: Some(dark_colors),
                min_supergroup_chat_boost_level: *min_megagroup_boost_levels.get(&id).unwrap_or(&0),
                min_channel_chat_boost_level: *min_broadcast_boost_levels.get(&id).unwrap_or(&0),
            });
        }
        let available_accent_color_ids: Vec<i32> =
            self.accent_color_ids.iter().map(|id| id.get()).collect();
        td_api::Update::updateProfileAccentColors(td_api::updateProfileAccentColors {
            colors,
            available_accent_color_ids,
        })
    }
}

/// `ThemeManager::ThemeSettings` (ThemeSettings.h) — the theme-settings
/// value class with its byte-exact store/parse (ThemeSettings.hpp).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ThemeSettings {
    accent_color: i32,
    message_accent_color: i32,
    has_message_accent_color: bool,
    background_info: Option<BackgroundInfo>,
    base_theme: BaseTheme,
    message_colors: Vec<i32>,
    animate_message_colors: bool,
}

impl ThemeSettings {
    /// `ThemeSettings::ThemeSettings(Td *, themeSettings)` (ThemeSettings.cpp:18-31)
    /// through the background seam; `None` settings keep the default (the
    /// C++ null-settings ctor body).
    pub fn from_telegram(
        env: &mut impl ThemeManagerEnv,
        settings: Option<telegram_api::ThemeSettings>,
        _is_no_file_pattern: bool,
    ) -> ThemeSettings {
        let Some(settings) = settings else {
            return ThemeSettings::default();
        };
        if settings.message_colors.len() > 4 {
            return ThemeSettings::default();
        }
        let accent_color = settings.accent_color;
        let has_message_accent_color = settings.flags & (1 << 3) != 0; // OUTBOX_ACCENT_COLOR_MASK
        let message_accent_color = if has_message_accent_color {
            settings.outbox_accent_color
        } else {
            accent_color
        };
        let background_info = BackgroundInfo::from_telegram(env, settings.wallpaper, true);
        let base_theme = settings
            .base_theme
            .as_ref()
            .map(get_base_theme)
            .unwrap_or(BaseTheme::Classic);
        ThemeSettings {
            accent_color,
            message_accent_color,
            has_message_accent_color,
            background_info,
            base_theme,
            message_colors: settings.message_colors,
            animate_message_colors: settings.message_colors_animated,
        }
    }

    /// `ThemeSettings::get_theme_settings_object` (ThemeSettings.cpp:33-39).
    pub fn get_theme_settings_object(
        &self,
        env: &mut impl ThemeManagerEnv,
    ) -> td_api::ThemeSettings {
        td_api::ThemeSettings {
            base_theme: Some(get_built_in_theme_object(self.base_theme)),
            accent_color: self.accent_color,
            background: self
                .background_info
                .as_ref()
                .and_then(|info| info.get_background_object(env)),
            outgoing_message_fill: BackgroundFill::get_background_fill_object_from_colors(
                &self.message_colors,
            ),
            animate_outgoing_message_fill: self.animate_message_colors,
            has_outgoing_message_accent_color: self.has_message_accent_color,
            outgoing_message_accent_color: self.message_accent_color,
        }
    }

    /// `ThemeSettings::is_empty` — equal to the default.
    pub fn is_empty(&self) -> bool {
        *self == ThemeSettings::default()
    }

    /// `ThemeSettings::are_dark` (ThemeSettings.h:41-43).
    pub fn are_dark(&self) -> bool {
        is_dark_base_theme(self.base_theme)
    }

    /// `ThemeSettings::store` (ThemeSettings.hpp:17-35) — the
    /// `background_info_.is_valid()` gate ANDs the env row resolvability
    /// until the M.16 row store lands (see the module divergences).
    pub(crate) fn store_inner(&self, env: &mut impl ThemeManagerEnv, out: &mut Vec<u8>) {
        let has_message_accent_color = self.has_message_accent_color;
        let mut background_stored = false;
        let mut background_bytes = Vec::new();
        if let Some(info) = &self.background_info
            && info.is_valid()
            && info.store(env, &mut background_bytes)
        {
            background_stored = true;
        }
        store_parse::store_flags(
            out,
            [
                self.animate_message_colors,
                has_message_accent_color,
                background_stored,
            ],
        );
        store_parse::store_i32(out, self.accent_color);
        if has_message_accent_color {
            store_parse::store_i32(out, self.message_accent_color);
        }
        if background_stored {
            out.extend_from_slice(&background_bytes);
        }
        store_parse::store_i32(out, self.base_theme as i32);
        store_parse::store_i32_vec(out, &self.message_colors);
    }

    /// `ThemeSettings::parse` (ThemeSettings.hpp:37-57).
    pub(crate) fn parse_inner(
        env: &mut impl ThemeManagerEnv,
        parser: &mut Parser,
    ) -> Result<ThemeSettings, &'static str> {
        let mut result = ThemeSettings::default();
        let [
            animate_message_colors,
            has_message_accent_color,
            has_background,
        ] = parser.parse_flags()?;
        result.animate_message_colors = animate_message_colors;
        result.accent_color = parser.i32_()?;
        if has_message_accent_color {
            result.has_message_accent_color = true;
            result.message_accent_color = parser.i32_()?;
        } else {
            result.message_accent_color = result.accent_color;
        }
        if has_background {
            result.background_info = Some(BackgroundInfo::parse(env, parser)?);
        }
        result.base_theme = match parser.i32_()? {
            0 => BaseTheme::Classic,
            1 => BaseTheme::Day,
            2 => BaseTheme::Night,
            3 => BaseTheme::Tinted,
            4 => BaseTheme::Arctic,
            _ => return Err("Invalid base theme"),
        };
        let size = parser.i32_()?;
        for _ in 0..size {
            result.message_colors.push(parser.i32_()?);
        }
        Result::Ok(result)
    }
}

/// The `G()`/`td_` surface of the manager. The four net queries ride
/// the dispatcher at the `ThemeEnv` arms; the `send_update` outbox is
/// drained by `crate::td::Td::flush_user_updates`.
pub trait ThemeManagerEnv {
    /// `td_->auth_manager_->is_authorized()`.
    fn is_authorized(&self) -> bool;

    /// `td_->auth_manager_->is_bot()`.
    fn is_bot(&self) -> bool;

    /// `G()->td_db()->get_binlog_pmc()->get`.
    fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;

    /// `G()->td_db()->get_binlog_pmc()->set`.
    fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]);

    /// `td_->option_manager_->get_option_integer(name)` (the C++ default 0).
    fn get_option_integer(&mut self, name: &str) -> i64;

    /// `send_closure(G()->td(), &Td::send_update, ...)`.
    fn send_update(&mut self, update: td_api::Update);

    /// `td_->user_manager_->on_get_users(users, source)`.
    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str);

    /// `td_->chat_manager_->on_get_chats(chats, source)`.
    fn on_get_chats(&mut self, chats: Vec<telegram_api::Chat>, source: &str);

    /// `BackgroundManager::on_get_background(BackgroundId(), string(),
    /// wallpaper, false, true, is_no_file_pattern)` — the id + type pair;
    /// `None` models the C++ empty return (invalid wallpaper).
    fn on_get_theme_background(
        &mut self,
        wallpaper: Option<telegram_api::WallPaper>,
        is_no_file_pattern: bool,
    ) -> Option<(BackgroundId, BackgroundType)> {
        let _ = (wallpaper, is_no_file_pattern);
        tdutils::log_info!("BackgroundManager::on_get_background — [blocked: M.16 Backgrounds]");
        None
    }

    /// `BackgroundManager::get_background_object(background_id, false,
    /// &background_type)` — the background object cache ([blocked: M.16]).
    fn get_theme_background_object(&mut self, info: &BackgroundInfo) -> Option<td_api::Background> {
        let _ = info;
        tdutils::log_info!(
            "BackgroundManager::get_background_object — [blocked: M.16 Backgrounds]"
        );
        None
    }

    /// `BackgroundManager::store_background(background_id, storer)` — the
    /// full `Background` row bytes ([blocked: M.16]); `false` models the
    /// unresolvable row (the theme settings then store background-less).
    fn store_background_row(&mut self, background_id: BackgroundId, out: &mut Vec<u8>) -> bool {
        let _ = (background_id, out);
        tdutils::log_info!("BackgroundManager::store_background — [blocked: M.16 Backgrounds]");
        false
    }

    /// `BackgroundManager::parse_background(background_id, parser)`.
    fn parse_background_row(&mut self, parser: &mut Parser) -> Result<BackgroundId, &'static str> {
        let _ = parser;
        tdutils::log_info!("BackgroundManager::parse_background — [blocked: M.16 Backgrounds]");
        Result::Ok(BackgroundId::new(0))
    }

    /// `StarGift(td, gift, true)` + `is_valid`/`is_unique` +
    /// `get_upgraded_gift_object` for the chat-theme gift variant
    /// ([blocked: M.42 StarGift]); `None` models the invalid bail.
    fn star_gift_object_for_theme(
        &mut self,
        gift: telegram_api::StarGift,
    ) -> Option<td_api::UpgradedGift> {
        let _ = gift;
        tdutils::log_info!("StarGift for the chat theme — [blocked: M.42 StarGift]");
        None
    }

    /// `GetChatThemesQuery::send` (ThemeManager.cpp:52-54) — the
    /// dispatcher arm lives at the `ThemeEnv` impl; the answer arrives
    /// through [`ThemeManager::on_get_chat_themes`].
    fn send_get_chat_themes_query(&mut self, hash: i64) {
        tdutils::log_info!("Send account.getChatThemes(hash = {hash}) — [net-inert]");
    }

    /// `GetPeerColorsQuery::send` (ThemeManager.cpp:78-80) — the
    /// dispatcher arm lives at the `ThemeEnv` impl; the answer arrives
    /// through [`ThemeManager::on_get_accent_colors`].
    fn send_get_peer_colors_query(&mut self, hash: i32) {
        tdutils::log_info!("Send help.getPeerColors(hash = {hash}) — [net-inert]");
    }

    /// `GetPeerProfileColorsQuery::send` (ThemeManager.cpp:104-106) —
    /// the dispatcher arm lives at the `ThemeEnv` impl; the answer
    /// arrives through [`ThemeManager::on_get_profile_accent_colors`].
    fn send_get_peer_profile_colors_query(&mut self, hash: i32) {
        tdutils::log_info!("Send help.getPeerProfileColors(hash = {hash}) — [net-inert]");
    }

    /// `GetUniqueGiftChatThemesQuery::send` (ThemeManager.cpp:130-132)
    /// — the answers drain through `Td::on_theme_net_answer`.
    fn send_get_unique_gift_chat_themes_query(&mut self, offset: &str, limit: i32) {
        tdutils::log_info!(
            "Send account.getUniqueGiftChatThemes(offset = {offset}, limit = {limit}) — [net-inert]"
        );
    }
}

/// The theme query kinds riding the dispatcher — the C++ ResultHandler
/// classes of ThemeManager.cpp:44-170.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ThemeNetKind {
    /// `GetChatThemesQuery` (cpp:44) — answers `account_Themes`.
    ChatThemes,
    /// `GetPeerColorsQuery` (cpp:70) — answers `help_PeerColors`.
    PeerColors,
    /// `GetPeerProfileColorsQuery` (cpp:96) — answers `help_PeerColors`.
    PeerProfileColors,
    /// `GetUniqueGiftChatThemesQuery` (cpp:122) — answers
    /// `account_ChatThemes`.
    UniqueGiftChatThemes,
}

/// The parked get-unique-gift-chat-themes promise (the C++ promise
/// rides the ResultHandler; answered by the net drain).
struct PendingGiftThemesQuery {
    promise: Promise<td_api::GiftChatThemes>,
}

/// `ThemeManager::DialogBoostAvailableCounts` (ThemeManager.h:46-51).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DialogBoostAvailableCounts {
    pub title_color_count: i32,
    pub accent_color_count: i32,
    pub profile_accent_color_count: i32,
    pub chat_theme_count: i32,
}

/// The port of `td::ThemeManager`.
#[derive(Default)]
pub struct ThemeManager {
    /// The start_up-once guard: the C++ runs the constructor + start_up
    /// once at Td start and the AuthManager.cpp:1552 `init()` after the
    /// sign-in; the collapsed Rust init keeps the first auth-true run as
    /// the effective one.
    is_inited: bool,
    /// `chat_themes_`.
    chat_themes: EmojiChatThemes,
    /// `accent_colors_`.
    accent_colors: AccentColors,
    /// `profile_accent_colors_`.
    profile_accent_colors: ProfileAccentColors,
    /// The parked get-unique-gift-chat-themes promises.
    pending_gift_theme_queries: Vec<PendingGiftThemesQuery>,
}

impl ThemeManager {
    /// `ThemeManager::ThemeManager` (ThemeManager.cpp:379-383).
    pub fn new() -> ThemeManager {
        ThemeManager::default()
    }

    /// `get_chat_themes_database_key` (ThemeManager.cpp:761-763).
    fn get_chat_themes_database_key() -> &'static str {
        "chat_themes"
    }

    /// `get_accent_colors_database_key` (ThemeManager.cpp:765-767).
    fn get_accent_colors_database_key() -> &'static str {
        "accent_colors"
    }

    /// `get_profile_accent_colors_database_key` (ThemeManager.cpp:769-771).
    fn get_profile_accent_colors_database_key() -> &'static str {
        "profile_accent_colors"
    }

    fn is_active(env: &impl ThemeManagerEnv) -> bool {
        env.is_authorized() && !env.is_bot()
    }

    /// `ThemeManager::init` (ThemeManager.cpp:439-452) + the constructor
    /// loads (cpp:380-381) — the collapsed Td-start/post-auth init
    /// (AuthManager.cpp:1552 fires it after every sign-in; idempotent).
    pub fn init(&mut self, env: &mut impl ThemeManagerEnv) {
        if !Self::is_active(env) {
            return;
        }
        if self.is_inited {
            return;
        }
        self.is_inited = true;

        // the constructor block (cpp:379-382)
        self.load_accent_colors(env);
        self.load_profile_accent_colors(env);
        // start_up → init (cpp:384-452)
        self.load_chat_themes(env);
        if self.chat_themes.hash == 0 {
            self.reload_chat_themes(env);
        }
        if self.accent_colors.hash == 0 {
            self.reload_accent_colors(env);
        }
        if self.profile_accent_colors.hash == 0 {
            self.reload_profile_accent_colors(env);
        }
    }

    /// `ThemeManager::load_chat_themes` (ThemeManager.cpp:388-403).
    fn load_chat_themes(&mut self, env: &mut impl ThemeManagerEnv) {
        if !Self::is_active(env) {
            return;
        }
        let log_event_string =
            env.binlog_pmc_get_raw(Self::get_chat_themes_database_key().as_bytes());
        if !log_event_string.is_empty() {
            // `log_event_parse` framing — the version prefix is consumed
            // by the parser constructor.
            let parsed = Parser::new_log_event(&log_event_string)
                .and_then(|mut parser| EmojiChatThemes::parse(env, &mut parser));
            match parsed {
                Result::Ok(chat_themes) => {
                    self.chat_themes = chat_themes;
                    self.send_update_chat_themes(env);
                }
                Result::Err(status) => {
                    tdutils::log_error!("Failed to parse chat themes from binlog: {status}");
                    self.chat_themes = EmojiChatThemes::default();
                }
            }
        }
    }

    /// `ThemeManager::load_accent_colors` (ThemeManager.cpp:405-420).
    fn load_accent_colors(&mut self, env: &mut impl ThemeManagerEnv) {
        if !Self::is_active(env) {
            return;
        }
        let log_event_string =
            env.binlog_pmc_get_raw(Self::get_accent_colors_database_key().as_bytes());
        if !log_event_string.is_empty() {
            let parsed = Parser::new_log_event(&log_event_string)
                .and_then(|mut parser| AccentColors::parse(&mut parser));
            match parsed {
                Result::Ok(accent_colors) => {
                    self.accent_colors = accent_colors;
                    self.send_update_accent_colors(env);
                }
                Result::Err(status) => {
                    tdutils::log_error!("Failed to parse accent colors from binlog: {status}");
                    self.accent_colors = AccentColors::default();
                }
            }
        }
    }

    /// `ThemeManager::load_profile_accent_colors` (ThemeManager.cpp:422-437).
    fn load_profile_accent_colors(&mut self, env: &mut impl ThemeManagerEnv) {
        if !Self::is_active(env) {
            return;
        }
        let log_event_string =
            env.binlog_pmc_get_raw(Self::get_profile_accent_colors_database_key().as_bytes());
        if !log_event_string.is_empty() {
            let parsed = Parser::new_log_event(&log_event_string)
                .and_then(|mut parser| ProfileAccentColors::parse(&mut parser));
            match parsed {
                Result::Ok(profile_accent_colors) => {
                    self.profile_accent_colors = profile_accent_colors;
                    self.send_update_profile_accent_colors(env);
                }
                Result::Err(status) => {
                    tdutils::log_error!(
                        "Failed to parse profile accent colors from binlog: {status}"
                    );
                    self.profile_accent_colors = ProfileAccentColors::default();
                }
            }
        }
    }

    /// `ThemeManager::save_chat_themes` (ThemeManager.cpp:773-775).
    fn save_chat_themes(&self, env: &mut impl ThemeManagerEnv) {
        // `log_event_store` framing — the version prefix leads each row.
        let mut out = Vec::new();
        store_parse::store_log_event_version(&mut out);
        self.chat_themes.store(env, &mut out);
        env.binlog_pmc_set_raw(Self::get_chat_themes_database_key().as_bytes(), &out);
    }

    /// `ThemeManager::save_accent_colors` (ThemeManager.cpp:777-780).
    fn save_accent_colors(&self, env: &mut impl ThemeManagerEnv) {
        let mut out = Vec::new();
        store_parse::store_log_event_version(&mut out);
        self.accent_colors.store(&mut out);
        env.binlog_pmc_set_raw(Self::get_accent_colors_database_key().as_bytes(), &out);
    }

    /// `ThemeManager::save_profile_accent_colors` (ThemeManager.cpp:782-785).
    fn save_profile_accent_colors(&self, env: &mut impl ThemeManagerEnv) {
        let mut out = Vec::new();
        store_parse::store_log_event_version(&mut out);
        self.profile_accent_colors.store(&mut out);
        env.binlog_pmc_set_raw(
            Self::get_profile_accent_colors_database_key().as_bytes(),
            &out,
        );
    }

    /// `ThemeManager::send_update_chat_themes` (ThemeManager.cpp:787-789)
    /// via `get_update_emoji_chat_themes_object` (cpp:668-671).
    fn send_update_chat_themes(&self, env: &mut impl ThemeManagerEnv) {
        let themes: Vec<td_api::EmojiChatTheme> = self
            .chat_themes
            .themes
            .iter()
            .map(|theme| td_api::EmojiChatTheme {
                name: theme.emoji.clone(),
                light_settings: Some(theme.light_theme.get_theme_settings_object(env)),
                dark_settings: Some(theme.dark_theme.get_theme_settings_object(env)),
            })
            .collect();
        env.send_update(td_api::Update::updateEmojiChatThemes(
            td_api::updateEmojiChatThemes {
                chat_themes: themes,
            },
        ));
    }

    /// `ThemeManager::send_update_accent_colors` (ThemeManager.cpp:791-793).
    fn send_update_accent_colors(&self, env: &mut impl ThemeManagerEnv) {
        env.send_update(self.accent_colors.get_update_accent_colors_object());
    }

    /// `ThemeManager::send_update_profile_accent_colors` (ThemeManager.cpp:795-797).
    fn send_update_profile_accent_colors(&self, env: &mut impl ThemeManagerEnv) {
        env.send_update(
            self.profile_accent_colors
                .get_update_profile_accent_colors_object(),
        );
    }

    /// `ThemeManager::reload_chat_themes` (ThemeManager.cpp:799-806).
    pub fn reload_chat_themes(&mut self, env: &mut impl ThemeManagerEnv) {
        env.send_get_chat_themes_query(self.chat_themes.hash);
    }

    /// `ThemeManager::on_get_chat_themes` (ThemeManager.cpp:808-862) — the
    /// answer handler; the query send rides the dispatcher.
    pub fn on_get_chat_themes(
        &mut self,
        env: &mut impl ThemeManagerEnv,
        result: Result<telegram_api::account_Themes, Status>,
    ) {
        let Ok(chat_themes) = result else {
            return;
        };
        let themes = match chat_themes {
            telegram_api::account_Themes::account_themesNotModified(_) => return,
            telegram_api::account_Themes::account_themes(themes) => themes,
        };
        self.chat_themes.hash = themes.hash;
        self.chat_themes.themes.clear();
        for theme in themes.themes {
            if !tdutils::emoji::is_emoji(&theme.emoticon) || !theme.for_chat {
                tdutils::log_error!("Receive an invalid chat theme");
                continue;
            }

            let mut was_light = false;
            let mut was_dark = false;
            let mut chat_theme = EmojiChatTheme {
                emoji: theme.emoticon.clone(),
                id: theme.id,
                light_theme: ThemeSettings::default(),
                dark_theme: ThemeSettings::default(),
            };
            for settings in theme.settings {
                let theme_settings = ThemeSettings::from_telegram(env, Some(settings), true);
                if theme_settings.is_empty() {
                    continue;
                }
                if theme_settings.are_dark() {
                    if !was_dark {
                        was_dark = true;
                        if chat_theme.dark_theme != theme_settings {
                            chat_theme.dark_theme = theme_settings;
                        }
                    }
                } else if !was_light {
                    was_light = true;
                    if chat_theme.light_theme != theme_settings {
                        chat_theme.light_theme = theme_settings;
                    }
                }
            }
            if chat_theme.light_theme.is_empty() || chat_theme.dark_theme.is_empty() {
                continue;
            }
            self.chat_themes.themes.push(chat_theme);
        }

        self.save_chat_themes(env);
        self.send_update_chat_themes(env);
    }

    /// `ThemeManager::reload_accent_colors` (ThemeManager.cpp:864-871).
    pub fn reload_accent_colors(&mut self, env: &mut impl ThemeManagerEnv) {
        env.send_get_peer_colors_query(self.accent_colors.hash);
    }

    /// `ThemeManager::on_update_accent_colors` (ThemeManager.cpp:536-570).
    fn on_update_accent_colors(
        &mut self,
        env: &mut impl ThemeManagerEnv,
        light_colors: AccentColorMap<Vec<i32>>,
        dark_colors: AccentColorMap<Vec<i32>>,
        accent_color_ids: Vec<AccentColorId>,
        min_broadcast_boost_levels: Vec<i32>,
        min_megagroup_boost_levels: Vec<i32>,
    ) -> bool {
        let are_equal = |lhs: &AccentColorMap<Vec<i32>>, rhs: &AccentColorMap<Vec<i32>>| {
            lhs.iter().all(|(key, value)| rhs.get(key) == Some(value))
        };
        if accent_color_ids == self.accent_colors.accent_color_ids
            && min_broadcast_boost_levels == self.accent_colors.min_broadcast_boost_levels
            && min_megagroup_boost_levels == self.accent_colors.min_megagroup_boost_levels
            && are_equal(&light_colors, &self.accent_colors.light_colors)
            && are_equal(&dark_colors, &self.accent_colors.dark_colors)
        {
            return false;
        }
        for (key, value) in light_colors.iter() {
            self.accent_colors.light_colors.set(key, value.clone());
        }
        for (key, value) in dark_colors.iter() {
            self.accent_colors.dark_colors.set(key, value.clone());
        }
        self.accent_colors.accent_color_ids = accent_color_ids;
        self.accent_colors.min_broadcast_boost_levels = min_broadcast_boost_levels;
        self.accent_colors.min_megagroup_boost_levels = min_megagroup_boost_levels;

        self.save_accent_colors(env);
        self.send_update_accent_colors(env);
        true
    }

    /// `ThemeManager::on_get_accent_colors` (ThemeManager.cpp:873-947).
    pub fn on_get_accent_colors(
        &mut self,
        env: &mut impl ThemeManagerEnv,
        result: Result<telegram_api::help_PeerColors, Status>,
    ) {
        let Ok(peer_colors) = result else {
            return;
        };
        let peer_colors = match peer_colors {
            telegram_api::help_PeerColors::help_peerColorsNotModified(_) => return,
            telegram_api::help_PeerColors::help_peerColors(peer_colors) => peer_colors,
        };
        let mut light_colors: AccentColorMap<Vec<i32>> = AccentColorMap::new();
        let mut dark_colors: AccentColorMap<Vec<i32>> = AccentColorMap::new();
        let mut accent_color_ids: Vec<AccentColorId> = Vec::new();
        let mut min_broadcast_boost_levels: Vec<i32> = Vec::new();
        let mut min_megagroup_boost_levels: Vec<i32> = Vec::new();
        for option in peer_colors.colors {
            // the C++ first gate: a present set must be the plain
            // peerColorSet kind (ThemeManager.cpp:890-895)
            let has_wrong_kind = |set: &Option<telegram_api::help_PeerColorSet>| {
                matches!(
                    set,
                    Some(telegram_api::help_PeerColorSet::help_peerColorProfileSet(_))
                )
            };
            if has_wrong_kind(&option.colors) || has_wrong_kind(&option.dark_colors) {
                tdutils::log_error!("Receive a peer color option with a wrong color set kind");
                continue;
            }
            let accent_color_id = AccentColorId::new(option.color_id);
            if !accent_color_id.is_valid()
                || accent_color_ids.contains(&accent_color_id)
                || (accent_color_id.is_built_in()
                    && (option.colors.is_some() || option.dark_colors.is_some()))
                || (!accent_color_id.is_built_in() && option.colors.is_none())
            {
                tdutils::log_error!("Receive an invalid peer color option");
                continue;
            }
            let mut is_valid = true;
            let mut current_light_colors: Vec<i32> = Vec::new();
            if let Some(telegram_api::help_PeerColorSet::help_peerColorSet(colors)) = &option.colors
            {
                current_light_colors = colors.colors.clone();
                if !are_colors_valid(&current_light_colors, 1, 3) {
                    is_valid = false;
                }
            }
            let mut current_dark_colors: Vec<i32> = Vec::new();
            if let Some(telegram_api::help_PeerColorSet::help_peerColorSet(colors)) =
                &option.dark_colors
            {
                current_dark_colors = colors.colors.clone();
                if !are_colors_valid(&current_dark_colors, 1, 3) {
                    is_valid = false;
                }
            }
            if !is_valid {
                tdutils::log_error!("Receive invalid colors");
                continue;
            }
            if !option.hidden {
                accent_color_ids.push(accent_color_id);
                min_broadcast_boost_levels.push(0.max(option.channel_min_level));
                min_megagroup_boost_levels.push(0.max(option.group_min_level));
            }
            if !current_light_colors.is_empty() {
                light_colors.set(accent_color_id, current_light_colors);
            }
            if !current_dark_colors.is_empty() {
                dark_colors.set(accent_color_id, current_dark_colors);
            }
        }

        let mut is_changed = false;
        if self.accent_colors.hash != peer_colors.hash {
            self.accent_colors.hash = peer_colors.hash;
            is_changed = true;
        }
        if !self.on_update_accent_colors(
            env,
            light_colors,
            dark_colors,
            accent_color_ids,
            min_broadcast_boost_levels,
            min_megagroup_boost_levels,
        ) && is_changed
        {
            self.save_accent_colors(env);
        }
    }

    /// `ThemeManager::reload_profile_accent_colors` (ThemeManager.cpp:949-956).
    pub fn reload_profile_accent_colors(&mut self, env: &mut impl ThemeManagerEnv) {
        env.send_get_peer_profile_colors_query(self.profile_accent_colors.hash);
    }

    /// `ThemeManager::get_profile_accent_color` (ThemeManager.cpp:958-968).
    fn get_profile_accent_color(
        color_set: telegram_api::help_peerColorProfileSet,
    ) -> ProfileAccentColor {
        ProfileAccentColor {
            palette_colors: color_set.palette_colors,
            background_colors: color_set.bg_colors,
            story_colors: color_set.story_colors,
        }
    }

    /// `ThemeManager::on_update_profile_accent_colors` (ThemeManager.cpp:572-607).
    fn on_update_profile_accent_colors(
        &mut self,
        env: &mut impl ThemeManagerEnv,
        light_colors: AccentColorMap<ProfileAccentColor>,
        dark_colors: AccentColorMap<ProfileAccentColor>,
        accent_color_ids: Vec<AccentColorId>,
        min_broadcast_boost_levels: Vec<i32>,
        min_megagroup_boost_levels: Vec<i32>,
    ) -> bool {
        let are_equal = |lhs: &AccentColorMap<ProfileAccentColor>,
                         rhs: &AccentColorMap<ProfileAccentColor>| {
            lhs.iter().all(|(key, value)| rhs.get(key) == Some(value))
        };
        if accent_color_ids == self.profile_accent_colors.accent_color_ids
            && min_broadcast_boost_levels == self.profile_accent_colors.min_broadcast_boost_levels
            && min_megagroup_boost_levels == self.profile_accent_colors.min_megagroup_boost_levels
            && are_equal(&light_colors, &self.profile_accent_colors.light_colors)
            && are_equal(&dark_colors, &self.profile_accent_colors.dark_colors)
        {
            return false;
        }
        for (key, value) in light_colors.iter() {
            self.profile_accent_colors
                .light_colors
                .set(key, value.clone());
        }
        for (key, value) in dark_colors.iter() {
            self.profile_accent_colors
                .dark_colors
                .set(key, value.clone());
        }
        self.profile_accent_colors.accent_color_ids = accent_color_ids;
        self.profile_accent_colors.min_broadcast_boost_levels = min_broadcast_boost_levels;
        self.profile_accent_colors.min_megagroup_boost_levels = min_megagroup_boost_levels;

        self.save_profile_accent_colors(env);
        self.send_update_profile_accent_colors(env);
        true
    }

    /// `ThemeManager::on_get_profile_accent_colors` (ThemeManager.cpp:970-1022).
    pub fn on_get_profile_accent_colors(
        &mut self,
        env: &mut impl ThemeManagerEnv,
        result: Result<telegram_api::help_PeerColors, Status>,
    ) {
        let Ok(peer_colors) = result else {
            return;
        };
        let peer_colors = match peer_colors {
            telegram_api::help_PeerColors::help_peerColorsNotModified(_) => return,
            telegram_api::help_PeerColors::help_peerColors(peer_colors) => peer_colors,
        };
        let mut light_colors: AccentColorMap<ProfileAccentColor> = AccentColorMap::new();
        let mut dark_colors: AccentColorMap<ProfileAccentColor> = AccentColorMap::new();
        let mut accent_color_ids: Vec<AccentColorId> = Vec::new();
        let mut min_broadcast_boost_levels: Vec<i32> = Vec::new();
        let mut min_megagroup_boost_levels: Vec<i32> = Vec::new();
        for option in peer_colors.colors {
            let accent_color_id = AccentColorId::new(option.color_id);
            let (Some(light), Some(dark)) = (option.colors, option.dark_colors) else {
                tdutils::log_error!("Receive a peer color option without color sets");
                continue;
            };
            let (light_set, dark_set) = match (light, dark) {
                (
                    telegram_api::help_PeerColorSet::help_peerColorProfileSet(light_set),
                    telegram_api::help_PeerColorSet::help_peerColorProfileSet(dark_set),
                ) => (light_set, dark_set),
                _ => {
                    tdutils::log_error!("Receive a peer color option with a wrong color set kind");
                    continue;
                }
            };
            if !accent_color_id.is_valid() || accent_color_ids.contains(&accent_color_id) {
                tdutils::log_error!("Receive an invalid peer color option");
                continue;
            }
            let current_light_color = Self::get_profile_accent_color(light_set);
            let current_dark_color = Self::get_profile_accent_color(dark_set);
            if !current_light_color.is_valid() || !current_dark_color.is_valid() {
                tdutils::log_error!("Receive invalid colors");
                continue;
            }
            if !option.hidden {
                accent_color_ids.push(accent_color_id);
                min_broadcast_boost_levels.push(0.max(option.channel_min_level));
                min_megagroup_boost_levels.push(0.max(option.group_min_level));
            }
            light_colors.set(accent_color_id, current_light_color);
            dark_colors.set(accent_color_id, current_dark_color);
        }

        let mut is_changed = false;
        if self.profile_accent_colors.hash != peer_colors.hash {
            self.profile_accent_colors.hash = peer_colors.hash;
            is_changed = true;
        }
        if !self.on_update_profile_accent_colors(
            env,
            light_colors,
            dark_colors,
            accent_color_ids,
            min_broadcast_boost_levels,
            min_megagroup_boost_levels,
        ) && is_changed
        {
            self.save_profile_accent_colors(env);
        }
    }

    /// `ThemeManager::on_update_theme` (ThemeManager.cpp:497-534) — the
    /// `UpdatesManager::on_update(updateTheme)` hook (the M.7 seam).
    pub fn on_update_theme(
        &mut self,
        env: &mut impl ThemeManagerEnv,
        theme: telegram_api::Theme,
        mut promise: Promise<()>,
    ) {
        let mut is_changed = false;
        let mut was_light = false;
        let mut was_dark = false;
        for chat_theme in &mut self.chat_themes.themes {
            if chat_theme.id == theme.id {
                for settings in &theme.settings {
                    let theme_settings =
                        ThemeSettings::from_telegram(env, Some(settings.clone()), true);
                    if theme_settings.is_empty() {
                        continue;
                    }
                    if theme_settings.are_dark() {
                        if !was_dark {
                            was_dark = true;
                            if chat_theme.dark_theme != theme_settings {
                                chat_theme.dark_theme = theme_settings;
                                is_changed = true;
                            }
                        }
                    } else if !was_light {
                        was_light = true;
                        if chat_theme.light_theme != theme_settings {
                            chat_theme.light_theme = theme_settings;
                            is_changed = true;
                        }
                    }
                }
            }
        }
        if is_changed {
            self.save_chat_themes(env);
            self.send_update_chat_themes(env);
        }
        promise.set_value(());
    }

    /// `ThemeManager::get_dialog_boost_available_count` (ThemeManager.cpp:458-495).
    pub fn get_dialog_boost_available_count(
        &self,
        env: &mut impl ThemeManagerEnv,
        level: i32,
        for_megagroup: bool,
    ) -> DialogBoostAvailableCounts {
        let mut result = DialogBoostAvailableCounts::default();
        let wallpaper_level_min = env.get_option_integer(if for_megagroup {
            "group_wallpaper_level_min"
        } else {
            "channel_wallpaper_level_min"
        });
        if level >= wallpaper_level_min as i32 {
            result.chat_theme_count = self.chat_themes.themes.len() as i32;
        }
        {
            let min_boost_levels = if for_megagroup {
                &self.accent_colors.min_megagroup_boost_levels
            } else {
                &self.accent_colors.min_broadcast_boost_levels
            };
            for (i, &min_boost_level) in min_boost_levels.iter().enumerate() {
                if level >= min_boost_level && min_boost_level != 0 {
                    result.accent_color_count += 1;

                    let accent_color_id = self.accent_colors.accent_color_ids[i];
                    if accent_color_id.is_built_in() {
                        result.title_color_count += 1;
                        continue;
                    }

                    let colors = self
                        .accent_colors
                        .light_colors
                        .get(accent_color_id)
                        .expect("accent color ids and light colors agree");
                    if colors.len() == 1 {
                        result.title_color_count += 1;
                    }
                }
            }
        }
        {
            let min_profile_boost_levels = if for_megagroup {
                &self.profile_accent_colors.min_megagroup_boost_levels
            } else {
                &self.profile_accent_colors.min_broadcast_boost_levels
            };
            for &min_boost_level in min_profile_boost_levels.iter() {
                if level >= min_boost_level && min_boost_level != 0 {
                    result.profile_accent_color_count += 1;
                }
            }
        }
        result
    }

    /// `ThemeManager::get_accent_color_id_object` (ThemeManager.cpp:639-650).
    pub fn get_accent_color_id_object(
        &self,
        env: &impl ThemeManagerEnv,
        accent_color_id: AccentColorId,
        fallback_accent_color_id: AccentColorId,
    ) -> i32 {
        if accent_color_id.is_valid()
            && (env.is_bot()
                || accent_color_id.is_built_in()
                || self.accent_colors.light_colors.contains(accent_color_id))
        {
            return accent_color_id.get();
        }
        if !fallback_accent_color_id.is_valid() {
            return 5; // blue
        }
        assert!(fallback_accent_color_id.is_built_in());
        fallback_accent_color_id.get()
    }

    /// `ThemeManager::get_profile_accent_color_id_object` (ThemeManager.cpp:652-660).
    pub fn get_profile_accent_color_id_object(
        &self,
        env: &impl ThemeManagerEnv,
        accent_color_id: AccentColorId,
    ) -> i32 {
        if !accent_color_id.is_valid() {
            return -1;
        }
        if env.is_bot()
            || self
                .profile_accent_colors
                .light_colors
                .contains(accent_color_id)
        {
            return accent_color_id.get();
        }
        -1
    }

    /// `ThemeManager::get_unique_gift_chat_themes` (ThemeManager.cpp:1024-1027)
    /// — the promise parks until [`Self::on_get_unique_gift_chat_themes`].
    pub fn get_unique_gift_chat_themes(
        &mut self,
        env: &mut impl ThemeManagerEnv,
        offset: &str,
        limit: i32,
        promise: Promise<td_api::GiftChatThemes>,
    ) {
        env.send_get_unique_gift_chat_themes_query(offset, limit);
        self.pending_gift_theme_queries
            .push(PendingGiftThemesQuery { promise });
    }

    /// `GetUniqueGiftChatThemesQuery::on_result` (ThemeManager.cpp:134-165)
    /// — the answer handler; on_error completes the parked promises with
    /// the query status (cpp:167-169).
    pub fn on_get_unique_gift_chat_themes(
        &mut self,
        env: &mut impl ThemeManagerEnv,
        result: Result<telegram_api::account_ChatThemes, Status>,
    ) {
        let pending = std::mem::take(&mut self.pending_gift_theme_queries);
        let chat_themes = match result {
            Result::Err(status) => {
                for mut query in pending {
                    query.promise.set_error(status.clone());
                }
                return;
            }
            Result::Ok(chat_themes) => chat_themes,
        };
        let themes = match chat_themes {
            telegram_api::account_ChatThemes::account_chatThemesNotModified(_) => {
                tdutils::log_error!("Receive account.chatThemesNotModified");
                for mut query in pending {
                    query
                        .promise
                        .set_error(Status::error(500, "Receive wrong server response"));
                }
                return;
            }
            telegram_api::account_ChatThemes::account_chatThemes(themes) => themes,
        };
        env.on_get_users(themes.users, "GetUniqueGiftChatThemesQuery");
        env.on_get_chats(themes.chats, "GetUniqueGiftChatThemesQuery");
        let mut gift_themes = Vec::new();
        for theme in themes.themes {
            let chat_theme = ChatTheme::from_telegram(env, theme);
            if !chat_theme.is_gift() {
                tdutils::log_error!("Receive a non-gift chat theme");
                continue;
            }
            gift_themes.push(chat_theme.get_gift_chat_theme_object(env));
        }
        let answer = td_api::GiftChatThemes {
            themes: gift_themes,
            next_offset: themes.next_offset,
        };
        for mut query in pending {
            query.promise.set_value(answer.clone());
        }
    }

    /// `ThemeManager::get_current_state` (ThemeManager.cpp:1029-1043) —
    /// implemented but never wired into the Rust getCurrentState block
    /// (see the module divergences: Requests.cpp 1.8.67 has no call).
    pub fn get_current_state(
        &self,
        env: &mut impl ThemeManagerEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        if !Self::is_active(env) {
            return;
        }

        if !self.chat_themes.themes.is_empty() {
            let themes: Vec<td_api::EmojiChatTheme> = self
                .chat_themes
                .themes
                .iter()
                .map(|theme| td_api::EmojiChatTheme {
                    name: theme.emoji.clone(),
                    light_settings: Some(theme.light_theme.get_theme_settings_object(env)),
                    dark_settings: Some(theme.dark_theme.get_theme_settings_object(env)),
                })
                .collect();
            updates.push(td_api::Update::updateEmojiChatThemes(
                td_api::updateEmojiChatThemes {
                    chat_themes: themes,
                },
            ));
        }
        if !self.accent_colors.accent_color_ids.is_empty() {
            updates.push(self.accent_colors.get_update_accent_colors_object());
        }
        if !self.profile_accent_colors.accent_color_ids.is_empty() {
            updates.push(
                self.profile_accent_colors
                    .get_update_profile_accent_colors_object(),
            );
        }
    }
}

/// `ThemeManager::get_theme_parameters_json_string` (ThemeManager.cpp:609-637)
/// — the pure static behind the synchronous getThemeParametersJsonString
/// (SynchronousRequests.cpp:325-329); the JSON object keeps the C++
/// insertion order and the `#rrggbb` lowercase color form.
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
    let fields = [
        ("bg_color", theme_parameters.background_color),
        (
            "secondary_bg_color",
            theme_parameters.secondary_background_color,
        ),
        ("text_color", theme_parameters.text_color),
        ("hint_color", theme_parameters.hint_color),
        ("link_color", theme_parameters.link_color),
        ("button_color", theme_parameters.button_color),
        ("button_text_color", theme_parameters.button_text_color),
        ("header_bg_color", theme_parameters.header_background_color),
        (
            "bottom_bar_bg_color",
            theme_parameters.bottom_bar_background_color,
        ),
        (
            "section_bg_color",
            theme_parameters.section_background_color,
        ),
        (
            "section_separator_color",
            theme_parameters.section_separator_color,
        ),
        ("accent_text_color", theme_parameters.accent_text_color),
        (
            "section_header_text_color",
            theme_parameters.section_header_text_color,
        ),
        ("subtitle_text_color", theme_parameters.subtitle_text_color),
        (
            "destructive_text_color",
            theme_parameters.destructive_text_color,
        ),
    ];
    let body: Vec<String> = fields
        .into_iter()
        .map(|(key, value)| format!("\"{key}\":\"{}\"", get_color(value)))
        .collect();
    format!("{{{}}}", body.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer_color::AccentColorId;
    use tdutils::promise::PromiseInterface;

    /// The recording env: pmc rows, sent updates, option values and the
    /// fired query seams.
    struct TestEnv {
        authorized: bool,
        bot: bool,
        pmc: HashMap<Vec<u8>, Vec<u8>>,
        options: HashMap<String, i64>,
        updates: Vec<td_api::Update>,
        queries: Vec<String>,
        gift_answer: Option<td_api::UpgradedGift>,
        background_rows: HashMap<i64, BackgroundType>,
    }

    impl Default for TestEnv {
        fn default() -> Self {
            TestEnv {
                authorized: true,
                bot: false,
                pmc: HashMap::new(),
                options: HashMap::new(),
                updates: Vec::new(),
                queries: Vec::new(),
                gift_answer: None,
                background_rows: HashMap::new(),
            }
        }
    }

    impl ThemeManagerEnv for TestEnv {
        fn is_authorized(&self) -> bool {
            self.authorized
        }

        fn is_bot(&self) -> bool {
            self.bot
        }

        fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
            self.pmc.get(key).cloned().unwrap_or_default()
        }

        fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
            self.pmc.insert(key.to_vec(), value.to_vec());
        }

        fn get_option_integer(&mut self, name: &str) -> i64 {
            *self.options.get(name).unwrap_or(&0)
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str) {
            self.queries.push(format!("{source}:{} users", users.len()));
        }

        fn on_get_chats(&mut self, chats: Vec<telegram_api::Chat>, source: &str) {
            self.queries.push(format!("{source}:{} chats", chats.len()));
        }

        fn on_get_theme_background(
            &mut self,
            wallpaper: Option<telegram_api::WallPaper>,
            is_no_file_pattern: bool,
        ) -> Option<(BackgroundId, BackgroundType)> {
            let wallpaper = match wallpaper? {
                telegram_api::WallPaper::wallPaperNoFile(wallpaper) => {
                    let settings = wallpaper.settings?;
                    let background_id = BackgroundId::new(wallpaper.id);
                    if !background_id.is_valid() || background_id.is_local() {
                        return None;
                    }
                    (
                        background_id,
                        BackgroundType::from_settings(true, is_no_file_pattern, Some(&settings)),
                    )
                }
                telegram_api::WallPaper::wallPaper(wallpaper) => {
                    let background_id = BackgroundId::new(wallpaper.id);
                    if !background_id.is_valid() || background_id.is_local() {
                        return None;
                    }
                    let is_pattern = wallpaper.pattern;
                    (
                        background_id,
                        BackgroundType::from_settings(
                            false,
                            is_pattern,
                            wallpaper.settings.as_ref(),
                        ),
                    )
                }
            };
            Some(wallpaper)
        }

        fn store_background_row(&mut self, background_id: BackgroundId, out: &mut Vec<u8>) -> bool {
            // the interim row: the id + the type bytes, symmetric with
            // parse_background_row; the real M.16 store replaces this
            if let Some(background_type) = self.background_rows.get(&background_id.get()) {
                store_parse::store_i64(out, background_id.get());
                background_type.store(out);
                true
            } else {
                false
            }
        }

        fn parse_background_row(
            &mut self,
            parser: &mut Parser,
        ) -> Result<BackgroundId, &'static str> {
            let background_id = BackgroundId::new(parser.i64_()?);
            let background_type = BackgroundType::parse(parser)?;
            self.background_rows
                .insert(background_id.get(), background_type);
            Result::Ok(background_id)
        }

        fn star_gift_object_for_theme(
            &mut self,
            gift: telegram_api::StarGift,
        ) -> Option<td_api::UpgradedGift> {
            self.gift_answer.as_ref()?;
            let _ = gift;
            Some(self.gift_answer.clone().expect("checked"))
        }

        fn send_get_chat_themes_query(&mut self, hash: i64) {
            self.queries.push(format!("chat_themes:{hash}"));
        }

        fn send_get_peer_colors_query(&mut self, hash: i32) {
            self.queries.push(format!("peer_colors:{hash}"));
        }

        fn send_get_peer_profile_colors_query(&mut self, hash: i32) {
            self.queries.push(format!("peer_profile_colors:{hash}"));
        }

        fn send_get_unique_gift_chat_themes_query(&mut self, offset: &str, limit: i32) {
            self.queries
                .push(format!("unique_gift_chat_themes:{offset}:{limit}"));
        }
    }

    fn sample_emoji_chat_theme(emoji: &str, theme_id: i64) -> EmojiChatTheme {
        let mut theme = EmojiChatTheme {
            emoji: emoji.to_owned(),
            id: theme_id,
            ..Default::default()
        };
        theme.light_theme.accent_color = 0xFF0000;
        theme.light_theme.base_theme = BaseTheme::Day;
        theme.light_theme.message_colors = vec![0x101010, 0x202020];
        theme.light_theme.message_accent_color = 0xEEEEEE;
        // the explicit flag gates the stored color (6dc3a1eb0)
        theme.light_theme.has_message_accent_color = true;
        theme.dark_theme.accent_color = 0x00FF00;
        theme.dark_theme.base_theme = BaseTheme::Night;
        theme
    }

    #[test]
    fn emoji_chat_themes_store_parse_round_trip() {
        let mut themes = EmojiChatThemes {
            hash: 0x1122334455667788,
            themes: Vec::new(),
        };
        themes.themes.push(sample_emoji_chat_theme("🔥", 1));
        themes.themes.push(sample_emoji_chat_theme("💜", 2));

        let mut env = TestEnv::default();
        let mut out = Vec::new();
        themes.store(&mut env, &mut out);
        let mut parser = Parser::new(&out);
        let parsed = EmojiChatThemes::parse(&mut env, &mut parser).expect("parse");
        // The parse normalizes an un-flagged `message_accent_color` to the
        // accent color (ThemeSettings.hpp:47-50).
        for theme in &mut themes.themes {
            if !theme.dark_theme.has_message_accent_color {
                theme.dark_theme.message_accent_color = theme.dark_theme.accent_color;
            }
            if !theme.light_theme.has_message_accent_color {
                theme.light_theme.message_accent_color = theme.light_theme.accent_color;
            }
        }
        assert_eq!(parsed, themes);
    }

    #[test]
    fn accent_colors_store_parse_round_trip_and_quirk() {
        let mut colors = AccentColors {
            hash: 12345,
            ..Default::default()
        };
        colors
            .light_colors
            .set(AccentColorId::new(7), vec![1, 2, 3]);
        colors.dark_colors.set(AccentColorId::new(7), vec![4, 5]);
        colors.accent_color_ids = vec![AccentColorId::new(0), AccentColorId::new(7)];
        colors.min_broadcast_boost_levels = vec![0, 4];
        colors.min_megagroup_boost_levels = vec![0, 5];

        let mut out = Vec::new();
        colors.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = AccentColors::parse(&mut parser).expect("parse");
        assert_eq!(parsed, colors);

        // the missing-min_broadcast `hash_ = 0` reset (ThemeManager.cpp:278-282)
        let mut stripped = colors.clone();
        stripped.min_broadcast_boost_levels.clear();
        let mut out = Vec::new();
        stripped.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = AccentColors::parse(&mut parser).expect("parse");
        assert_eq!(parsed.hash, 0);
        assert_eq!(parsed.light_colors, colors.light_colors);
    }

    #[test]
    fn profile_accent_colors_store_parse_round_trip_and_quirks() {
        let mut colors = ProfileAccentColors {
            hash: -77,
            ..Default::default()
        };
        let color = ProfileAccentColor {
            palette_colors: vec![0xFF0000],
            background_colors: vec![0x00FF00, 0x0000FF],
            story_colors: vec![0x111111, 0x222222],
        };
        colors
            .light_colors
            .set(AccentColorId::new(10), color.clone());
        colors.dark_colors.set(AccentColorId::new(10), color);
        colors.accent_color_ids = vec![AccentColorId::new(10)];
        colors.min_broadcast_boost_levels = vec![1];
        colors.min_megagroup_boost_levels = vec![2];

        let mut out = Vec::new();
        colors.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = ProfileAccentColors::parse(&mut parser).expect("parse");
        assert_eq!(parsed, colors);

        // both boost-level vectors missing → both quirk branches reset the hash
        let mut stripped = colors.clone();
        stripped.min_broadcast_boost_levels.clear();
        stripped.min_megagroup_boost_levels.clear();
        let mut out = Vec::new();
        stripped.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = ProfileAccentColors::parse(&mut parser).expect("parse");
        assert_eq!(parsed.hash, 0);
    }

    #[test]
    fn background_type_store_parse_round_trip() {
        let settings = telegram_api::WallPaperSettings {
            flags: 1 | (1 << 3) | (1 << 4) | (1 << 2),
            blur: false,
            motion: true,
            background_color: 0x336699,
            second_background_color: 0x663311,
            third_background_color: 0,
            fourth_background_color: 0,
            intensity: 33,
            rotation: 90,
            emoticon: String::new(),
        };
        let wallpaper = BackgroundType::from_settings(false, false, Some(&settings));
        assert_eq!(
            wallpaper.get_background_type_object(),
            td_api::BackgroundType::backgroundTypeWallpaper(td_api::backgroundTypeWallpaper {
                is_blurred: false,
                is_moving: true
            })
        );

        let pattern = BackgroundType::from_settings(false, true, Some(&settings));
        let mut out = Vec::new();
        pattern.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = BackgroundType::parse(&mut parser).expect("parse");
        assert_eq!(parsed, pattern);

        let chat_theme_settings = telegram_api::WallPaperSettings {
            flags: 1,
            blur: false,
            motion: false,
            background_color: 0x336699,
            second_background_color: 0,
            third_background_color: 0,
            fourth_background_color: 0,
            intensity: 0,
            rotation: 0,
            emoticon: "🔥".to_owned(),
        };
        let chat_theme = BackgroundType::from_settings(true, false, Some(&chat_theme_settings));
        assert!(matches!(
            chat_theme.get_background_type_object(),
            td_api::BackgroundType::backgroundTypeChatTheme(_)
        ));

        let fill = BackgroundType::from_settings(true, false, Some(&settings));
        assert!(!fill.has_file());
        assert_eq!(fill.get_dark_theme_dimming(), 33);
    }

    #[test]
    fn background_fill_from_wall_paper_settings() {
        // gradient: second color + rotation
        let gradient = telegram_api::WallPaperSettings {
            flags: 1 | (1 << 4),
            blur: false,
            motion: false,
            background_color: 0xABCDEF,
            second_background_color: 0x123456,
            third_background_color: 0,
            fourth_background_color: 0,
            intensity: 0,
            rotation: 45,
            emoticon: String::new(),
        };
        let fill = BackgroundFill::from_wall_paper_settings(Some(&gradient));
        assert_eq!(fill.get_type(), BackgroundFillType::Gradient);
        assert_eq!(
            fill.get_background_fill_object(),
            td_api::BackgroundFill::backgroundFillGradient(td_api::backgroundFillGradient {
                top_color: 0xABCDEF,
                bottom_color: 0x123456,
                rotation_angle: 45,
            })
        );

        // freeform: third color present; the out-of-range alpha clamps
        let freeform = telegram_api::WallPaperSettings {
            flags: 1 | (1 << 4) | (1 << 5),
            blur: false,
            motion: false,
            background_color: -0x2000000, // clamped to 0
            second_background_color: 0x123456,
            third_background_color: -0x100,
            fourth_background_color: 0,
            intensity: 0,
            rotation: 0,
            emoticon: String::new(),
        };
        let fill = BackgroundFill::from_wall_paper_settings(Some(&freeform));
        assert_eq!(fill.get_type(), BackgroundFillType::FreeformGradient);
        assert_eq!(fill.top_color, 0);

        // solid: only the first color
        let solid = telegram_api::WallPaperSettings {
            flags: 1,
            blur: false,
            motion: false,
            background_color: 0x010203,
            second_background_color: 0,
            third_background_color: 0,
            fourth_background_color: 0,
            intensity: 0,
            rotation: 0,
            emoticon: String::new(),
        };
        let fill = BackgroundFill::from_wall_paper_settings(Some(&solid));
        assert_eq!(fill.get_type(), BackgroundFillType::Solid);
    }

    #[test]
    fn theme_settings_from_telegram_and_object() {
        let mut env = TestEnv::default();
        let settings = telegram_api::ThemeSettings {
            flags: (1 << 3) | 1, // outbox accent + message colors
            message_colors_animated: true,
            base_theme: Some(telegram_api::BaseTheme::baseThemeNight(
                telegram_api::baseThemeNight,
            )),
            accent_color: 0x102030,
            outbox_accent_color: 0x405060,
            message_colors: vec![0x010203, 0x040506],
            wallpaper: None,
        };
        let theme_settings = ThemeSettings::from_telegram(&mut env, Some(settings.clone()), true);
        assert!(theme_settings.are_dark());
        assert!(!theme_settings.is_empty());
        assert_eq!(theme_settings.accent_color, 0x102030);
        assert_eq!(theme_settings.message_accent_color, 0x405060);
        assert!(theme_settings.background_info.is_none());

        let object = theme_settings.get_theme_settings_object(&mut env);
        assert!(object.background.is_none());
        assert_eq!(object.outgoing_message_accent_color, 0x405060);
        assert!(object.animate_outgoing_message_fill);
        assert!(matches!(
            object.outgoing_message_fill,
            Some(td_api::BackgroundFill::backgroundFillGradient(_))
        ));

        // the >4 message colors gate resets to default (ThemeSettings.cpp:21)
        let mut wide = settings;
        wide.message_colors = vec![1, 2, 3, 4, 5];
        assert!(ThemeSettings::from_telegram(&mut env, Some(wide), true).is_empty());

        // the null settings ctor
        assert!(ThemeSettings::from_telegram(&mut env, None, true).is_empty());
    }

    #[test]
    fn theme_settings_store_parse_round_trip_with_background() {
        let mut env = TestEnv::default();
        let wallpaper = telegram_api::WallPaper::wallPaperNoFile(telegram_api::wallPaperNoFile {
            id: 0x1_0000_0000, // not local, valid
            flags: 1 << 1,
            default: false,
            dark: false,
            settings: Some(telegram_api::WallPaperSettings {
                flags: 1,
                blur: false,
                motion: false,
                background_color: 0xABCDEF,
                second_background_color: 0,
                third_background_color: 0,
                fourth_background_color: 0,
                intensity: 0,
                rotation: 0,
                emoticon: String::new(),
            }),
        });
        let info =
            BackgroundInfo::from_telegram(&mut env, Some(wallpaper), true).expect("background");
        assert!(info.is_valid());

        // the store leg resolves the background row through the env (the
        // C++ BackgroundManager cache); prime it with the row from_telegram
        // registered
        env.background_rows
            .insert(0x1_0000_0000, info.background_type.clone());

        let settings = ThemeSettings {
            background_info: Some(info),
            accent_color: 7,
            ..Default::default()
        };

        let mut out = Vec::new();
        settings.store_inner(&mut env, &mut out);
        let mut parser = Parser::new(&out);
        let parsed = ThemeSettings::parse_inner(&mut env, &mut parser).expect("parse");
        // The parse normalizes `message_accent_color` to `accent_color`
        // when the explicit flag is absent (ThemeSettings.hpp:47-50) —
        // the C++ round trip has the same shape.
        let mut expected = settings.clone();
        expected.has_message_accent_color = false;
        expected.message_accent_color = expected.accent_color;
        assert_eq!(parsed, expected);
    }

    #[test]
    fn init_loads_pmc_sends_updates_and_reloads_once() {
        let mut env = TestEnv::default();

        // preset the accent colors row so the load path sends its update
        let mut colors = AccentColors {
            hash: 9,
            ..Default::default()
        };
        colors.accent_color_ids = vec![AccentColorId::new(7)];
        colors
            .light_colors
            .set(AccentColorId::new(7), vec![0x202020]);
        colors
            .dark_colors
            .set(AccentColorId::new(7), vec![0x202020]);
        colors.min_broadcast_boost_levels = vec![3];
        colors.min_megagroup_boost_levels = vec![3];
        let mut row = Vec::new();
        crate::store_parse::store_log_event_version(&mut row);
        colors.store(&mut row);
        env.binlog_pmc_set_raw("accent_colors".as_bytes(), &row);

        let mut manager = ThemeManager::new();
        manager.init(&mut env);
        assert!(manager.is_inited);
        assert_eq!(env.updates.len(), 1);
        assert!(matches!(
            env.updates[0],
            td_api::Update::updateAccentColors(_)
        ));
        // all three planes were loaded; only the hash-0 ones reload (the
        // accent row carries hash 9, so peer_colors stays put)
        assert_eq!(env.queries, vec!["chat_themes:0", "peer_profile_colors:0"]);

        // the second init is the start_up-once collapse: no re-loads
        manager.init(&mut env);
        assert_eq!(env.updates.len(), 1);
        assert_eq!(env.queries.len(), 2);
    }

    #[test]
    fn init_gates_on_auth_and_bot() {
        let mut env = TestEnv {
            authorized: false,
            ..Default::default()
        };
        let mut manager = ThemeManager::new();
        manager.init(&mut env);
        assert!(!manager.is_inited);
        assert!(env.updates.is_empty());
        assert!(env.queries.is_empty());

        let mut env = TestEnv {
            bot: true,
            ..Default::default()
        };
        let mut manager = ThemeManager::new();
        manager.init(&mut env);
        assert!(!manager.is_inited);
    }

    #[test]
    fn corrupt_pmc_resets_to_default() {
        let mut env = TestEnv::default();
        env.binlog_pmc_set_raw("chat_themes".as_bytes(), &[1, 2, 3]);
        let mut manager = ThemeManager::new();
        manager.init(&mut env);
        assert!(manager.chat_themes.themes.is_empty());
        assert!(env.updates.is_empty());
    }

    #[test]
    fn on_get_chat_themes_filters_and_saves() {
        let mut env = TestEnv::default();
        let valid_settings = vec![
            telegram_api::ThemeSettings {
                flags: 0,
                message_colors_animated: false,
                base_theme: Some(telegram_api::BaseTheme::baseThemeDay(
                    telegram_api::baseThemeDay,
                )),
                accent_color: 1,
                outbox_accent_color: 1,
                message_colors: vec![],
                wallpaper: None,
            },
            telegram_api::ThemeSettings {
                flags: 0,
                message_colors_animated: false,
                base_theme: Some(telegram_api::BaseTheme::baseThemeNight(
                    telegram_api::baseThemeNight,
                )),
                accent_color: 2,
                outbox_accent_color: 2,
                message_colors: vec![],
                wallpaper: None,
            },
        ];
        let answer = telegram_api::account_Themes::account_themes(telegram_api::account_themes {
            hash: 4242,
            themes: vec![
                telegram_api::Theme {
                    flags: 1 << 5, // for_chat
                    creator: false,
                    default: false,
                    for_chat: true,
                    id: 1,
                    access_hash: 0,
                    slug: "slug".to_owned(),
                    title: "title".to_owned(),
                    document: None,
                    settings: valid_settings.clone(),
                    emoticon: "\u{1F525}".to_owned(),
                    installs_count: 0,
                },
                // not for_chat → dropped
                telegram_api::Theme {
                    flags: 0,
                    creator: false,
                    default: false,
                    for_chat: false,
                    id: 2,
                    access_hash: 0,
                    slug: "slug".to_owned(),
                    title: "title".to_owned(),
                    document: None,
                    settings: valid_settings.clone(),
                    emoticon: "\u{1F49C}".to_owned(),
                    installs_count: 0,
                },
                // not an emoji → dropped
                telegram_api::Theme {
                    flags: 1 << 5,
                    creator: false,
                    default: false,
                    for_chat: true,
                    id: 3,
                    access_hash: 0,
                    slug: "slug".to_owned(),
                    title: "title".to_owned(),
                    document: None,
                    settings: valid_settings,
                    emoticon: "plain".to_owned(),
                    installs_count: 0,
                },
            ],
        });

        let mut manager = ThemeManager::new();
        manager.on_get_chat_themes(&mut env, Result::Ok(answer));
        assert_eq!(manager.chat_themes.hash, 4242);
        assert_eq!(manager.chat_themes.themes.len(), 1);
        assert_eq!(manager.chat_themes.themes[0].emoji, "\u{1F525}");
        assert!(!manager.chat_themes.themes[0].light_theme.is_empty());
        assert!(!manager.chat_themes.themes[0].dark_theme.is_empty());
        // saved + update sent
        assert!(env.pmc.contains_key("chat_themes".as_bytes()));
        assert_eq!(env.updates.len(), 1);
        assert!(matches!(
            env.updates[0],
            td_api::Update::updateEmojiChatThemes(_)
        ));

        // notModified is a no-op
        let before = env.updates.len();
        manager.on_get_chat_themes(
            &mut env,
            Result::Ok(telegram_api::account_Themes::account_themesNotModified(
                telegram_api::account_themesNotModified,
            )),
        );
        assert_eq!(env.updates.len(), before);

        // an error is dropped
        manager.on_get_chat_themes(&mut env, Result::Err(Status::error(400, "oops")));
        assert_eq!(env.updates.len(), before);
    }

    #[test]
    fn on_update_theme_replaces_settings() {
        let mut env = TestEnv::default();
        let mut manager = ThemeManager::new();
        let mut theme = telegram_api::Theme {
            flags: 1 << 5,
            creator: false,
            default: false,
            for_chat: true,
            id: 1,
            access_hash: 0,
            slug: String::new(),
            title: String::new(),
            document: None,
            settings: vec![
                telegram_api::ThemeSettings {
                    flags: 0,
                    message_colors_animated: false,
                    base_theme: Some(telegram_api::BaseTheme::baseThemeDay(
                        telegram_api::baseThemeDay,
                    )),
                    accent_color: 1,
                    outbox_accent_color: 1,
                    message_colors: vec![],
                    wallpaper: None,
                },
                telegram_api::ThemeSettings {
                    flags: 0,
                    message_colors_animated: false,
                    base_theme: Some(telegram_api::BaseTheme::baseThemeNight(
                        telegram_api::baseThemeNight,
                    )),
                    accent_color: 2,
                    outbox_accent_color: 2,
                    message_colors: vec![],
                    wallpaper: None,
                },
            ],
            emoticon: "\u{1F525}".to_owned(),
            installs_count: 0,
        };
        manager.on_get_chat_themes(
            &mut env,
            Result::Ok(telegram_api::account_Themes::account_themes(
                telegram_api::account_themes {
                    hash: 1,
                    themes: vec![theme.clone()],
                },
            )),
        );
        let updates_after_load = env.updates.len();
        assert_eq!(manager.chat_themes.themes[0].light_theme.accent_color, 1);

        // change the light accent color for the same theme id
        theme.settings[0].accent_color = 99;
        let (promise, recorder) = recording_promise();
        manager.on_update_theme(&mut env, theme.clone(), promise);
        assert_eq!(manager.chat_themes.themes[0].light_theme.accent_color, 99);
        assert_eq!(env.updates.len(), updates_after_load + 1);
        assert!(env.pmc.contains_key("chat_themes".as_bytes()));
        assert!(recorder.0.lock().unwrap().is_some());

        // an unrelated id changes nothing
        theme.id = 999;
        let (promise, _recorder) = recording_promise();
        manager.on_update_theme(&mut env, theme, promise);
        assert_eq!(manager.chat_themes.themes[0].light_theme.accent_color, 99);
        assert_eq!(env.updates.len(), updates_after_load + 1);
    }

    #[test]
    fn on_get_accent_colors_collects_options() {
        let mut env = TestEnv::default();
        let answer =
            telegram_api::help_PeerColors::help_peerColors(telegram_api::help_peerColors {
                hash: 7,
                colors: vec![
                    // a valid non-hidden accent color
                    telegram_api::help_PeerColorOption {
                        flags: 0,
                        hidden: false,
                        color_id: 10,
                        colors: Some(telegram_api::help_PeerColorSet::help_peerColorSet(
                            telegram_api::help_peerColorSet {
                                colors: vec![1, 2, 3],
                            },
                        )),
                        dark_colors: Some(telegram_api::help_PeerColorSet::help_peerColorSet(
                            telegram_api::help_peerColorSet { colors: vec![4] },
                        )),
                        channel_min_level: -5,
                        group_min_level: 2,
                    },
                    // a built-in id carrying colors → rejected
                    telegram_api::help_PeerColorOption {
                        flags: 0,
                        hidden: false,
                        color_id: 3,
                        colors: Some(telegram_api::help_PeerColorSet::help_peerColorSet(
                            telegram_api::help_peerColorSet { colors: vec![1] },
                        )),
                        dark_colors: None,
                        channel_min_level: 0,
                        group_min_level: 0,
                    },
                    // a hidden option contributes its colors but not the ids
                    telegram_api::help_PeerColorOption {
                        flags: 1,
                        hidden: true,
                        color_id: 11,
                        colors: Some(telegram_api::help_PeerColorSet::help_peerColorSet(
                            telegram_api::help_peerColorSet { colors: vec![9] },
                        )),
                        dark_colors: Some(telegram_api::help_PeerColorSet::help_peerColorSet(
                            telegram_api::help_peerColorSet { colors: vec![9] },
                        )),
                        channel_min_level: 0,
                        group_min_level: 0,
                    },
                ],
            });

        let mut manager = ThemeManager::new();
        manager.on_get_accent_colors(&mut env, Result::Ok(answer));
        assert_eq!(manager.accent_colors.hash, 7);
        assert_eq!(
            manager.accent_colors.accent_color_ids,
            vec![AccentColorId::new(10)]
        );
        assert_eq!(manager.accent_colors.min_broadcast_boost_levels, vec![0]);
        assert_eq!(manager.accent_colors.min_megagroup_boost_levels, vec![2]);
        assert_eq!(
            manager
                .accent_colors
                .light_colors
                .get(AccentColorId::new(10)),
            Some(&vec![1, 2, 3])
        );
        assert!(
            manager
                .accent_colors
                .light_colors
                .contains(AccentColorId::new(11))
        );
        assert_eq!(env.updates.len(), 1);
        assert!(env.pmc.contains_key("accent_colors".as_bytes()));
    }

    #[test]
    fn accent_color_id_object_rules() {
        let mut env = TestEnv::default();
        let mut manager = ThemeManager::new();
        manager
            .accent_colors
            .light_colors
            .set(AccentColorId::new(50), vec![1, 2, 3]);

        // a known non-built-in id passes through
        assert_eq!(
            manager.get_accent_color_id_object(&env, AccentColorId::new(50), AccentColorId::new(0)),
            50
        );
        // a built-in id always passes
        assert_eq!(
            manager.get_accent_color_id_object(&env, AccentColorId::new(3), AccentColorId::new(0)),
            3
        );
        // unknown → the built-in fallback
        assert_eq!(
            manager.get_accent_color_id_object(&env, AccentColorId::new(51), AccentColorId::new(2)),
            2
        );
        // unknown, invalid fallback → the blue default
        assert_eq!(
            manager.get_accent_color_id_object(
                &env,
                AccentColorId::new(51),
                AccentColorId::default()
            ),
            5
        );

        // bots pass everything through
        env.bot = true;
        assert_eq!(
            manager.get_accent_color_id_object(
                &env,
                AccentColorId::new(51),
                AccentColorId::default()
            ),
            51
        );
        assert_eq!(
            manager.get_profile_accent_color_id_object(&env, AccentColorId::new(51)),
            51
        );
        // an invalid profile id is -1 even for bots
        assert_eq!(
            manager.get_profile_accent_color_id_object(&env, AccentColorId::default()),
            -1
        );
    }

    #[test]
    fn get_dialog_boost_available_count_counts() {
        let mut env = TestEnv::default();
        env.options
            .insert("channel_wallpaper_level_min".to_owned(), 5);
        let mut manager = ThemeManager::new();
        manager
            .chat_themes
            .themes
            .push(sample_emoji_chat_theme("🔥", 1));

        // an unknown accent color id keeps the count consistent: the C++
        // CHECKs the id/light-colors agreement, so seed both planes
        manager.accent_colors.accent_color_ids =
            vec![AccentColorId::new(1), AccentColorId::new(20)];
        manager.accent_colors.min_broadcast_boost_levels = vec![1, 2];
        manager.accent_colors.min_megagroup_boost_levels = vec![1, 9];
        manager
            .accent_colors
            .light_colors
            .set(AccentColorId::new(20), vec![1, 2]);
        manager
            .accent_colors
            .dark_colors
            .set(AccentColorId::new(20), vec![1, 2]);
        manager.profile_accent_colors.accent_color_ids = vec![AccentColorId::new(30)];
        manager.profile_accent_colors.min_broadcast_boost_levels = vec![3];

        // below the wallpaper gate: no theme count
        let counts = manager.get_dialog_boost_available_count(&mut env, 4, false);
        assert_eq!(counts.chat_theme_count, 0);
        // level 1: below the wallpaper gate (5), so no theme count;
        // built-in id 1 → title+accent; the second id needs level 2
        let counts = manager.get_dialog_boost_available_count(&mut env, 1, false);
        assert_eq!(counts.chat_theme_count, 0);
        assert_eq!(counts.accent_color_count, 1);
        assert_eq!(counts.title_color_count, 1);
        assert_eq!(counts.profile_accent_color_count, 0);
        // level 3: both accents (20 is multi-color → no title) + profile
        let counts = manager.get_dialog_boost_available_count(&mut env, 3, false);
        assert_eq!(counts.accent_color_count, 2);
        assert_eq!(counts.title_color_count, 1);
        assert_eq!(counts.profile_accent_color_count, 1);
        // the megagroup plane gates id 20 behind level 9
        let counts = manager.get_dialog_boost_available_count(&mut env, 3, true);
        assert_eq!(counts.accent_color_count, 1);
    }

    #[test]
    fn theme_parameters_json_string_exact() {
        let parameters = td_api::ThemeParameters {
            background_color: 0x000000,
            secondary_background_color: 0xFFFFFF,
            header_background_color: 0xABCDEF,
            bottom_bar_background_color: 0x000ABC,
            section_background_color: 0,
            section_separator_color: 0,
            text_color: 0x123456,
            accent_text_color: 0,
            subtitle_text_color: 0,
            destructive_text_color: 0,
            hint_color: 0,
            link_color: 0,
            button_color: 0,
            button_text_color: 0,
            section_header_text_color: 0,
        };
        assert_eq!(
            get_theme_parameters_json_string(&parameters),
            "{\"bg_color\":\"#000000\",\"secondary_bg_color\":\"#ffffff\",\"text_color\":\"#123456\",\
             \"hint_color\":\"#000000\",\"link_color\":\"#000000\",\"button_color\":\"#000000\",\
             \"button_text_color\":\"#000000\",\"header_bg_color\":\"#abcdef\",\"bottom_bar_bg_color\":\"#000abc\",\
             \"section_bg_color\":\"#000000\",\"section_separator_color\":\"#000000\",\"accent_text_color\":\"#000000\",\
             \"section_header_text_color\":\"#000000\",\"subtitle_text_color\":\"#000000\",\"destructive_text_color\":\"#000000\"}"
        );
    }

    #[test]
    fn on_get_unique_gift_chat_themes_completes_the_parked_promises() {
        let mut env = TestEnv::default();
        let mut manager = ThemeManager::new();

        let (promise, recorder) = recording_promise();
        manager.get_unique_gift_chat_themes(&mut env, "", 10, promise);
        assert_eq!(env.queries, vec!["unique_gift_chat_themes::10"]);

        let emoji_theme = telegram_api::ChatTheme::chatTheme(telegram_api::chatTheme {
            emoticon: "\u{1F525}".to_owned(),
        });
        env.gift_answer = Some(td_api::UpgradedGift::default());
        let answer = telegram_api::account_ChatThemes::account_chatThemes(
            telegram_api::account_chatThemes {
                flags: 1,
                hash: 0,
                themes: vec![
                    // the gift variant (the env seam supplies the gift object)
                    telegram_api::ChatTheme::chatThemeUniqueGift(
                        telegram_api::chatThemeUniqueGift {
                            gift: Some(telegram_api::StarGift::starGift(telegram_api::starGift {
                                id: 1,
                                ..Default::default()
                            })),
                            theme_settings: vec![
                                telegram_api::ThemeSettings {
                                    flags: 0,
                                    message_colors_animated: false,
                                    base_theme: Some(telegram_api::BaseTheme::baseThemeDay(
                                        telegram_api::baseThemeDay,
                                    )),
                                    accent_color: 1,
                                    outbox_accent_color: 1,
                                    message_colors: vec![],
                                    wallpaper: None,
                                },
                                telegram_api::ThemeSettings {
                                    flags: 0,
                                    message_colors_animated: false,
                                    base_theme: Some(telegram_api::BaseTheme::baseThemeNight(
                                        telegram_api::baseThemeNight,
                                    )),
                                    accent_color: 2,
                                    outbox_accent_color: 2,
                                    message_colors: vec![],
                                    wallpaper: None,
                                },
                            ],
                        },
                    ),
                    // the emoji variant → dropped
                    emoji_theme,
                ],
                chats: vec![],
                users: vec![],
                next_offset: "next".to_owned(),
            },
        );

        manager.on_get_unique_gift_chat_themes(&mut env, Result::Ok(answer));
        assert_eq!(
            env.queries,
            vec![
                "unique_gift_chat_themes::10",
                "GetUniqueGiftChatThemesQuery:0 users",
                "GetUniqueGiftChatThemesQuery:0 chats"
            ]
        );
        let answer = recorder
            .0
            .lock()
            .unwrap()
            .clone()
            .expect("promise completed");
        assert_eq!(answer.next_offset, "next");
        assert_eq!(answer.themes.len(), 1);
        assert!(manager.pending_gift_theme_queries.is_empty());
    }

    #[test]
    fn chat_theme_from_telegram_variants() {
        let mut env = TestEnv::default();

        // default: empty emoticon
        let theme = ChatTheme::from_telegram(
            &mut env,
            telegram_api::ChatTheme::chatTheme(telegram_api::chatTheme {
                emoticon: String::new(),
            }),
        );
        assert!(theme.is_default());

        // emoji
        let theme = ChatTheme::from_telegram(
            &mut env,
            telegram_api::ChatTheme::chatTheme(telegram_api::chatTheme {
                emoticon: "\u{1F525}".to_owned(),
            }),
        );
        assert_eq!(
            theme.get_chat_theme_object(&mut env),
            Some(td_api::ChatTheme::chatThemeEmoji(td_api::chatThemeEmoji {
                name: "\u{1F525}".to_owned()
            }))
        );

        // the static emoji ctor
        assert!(ChatTheme::emoji("").is_default());
        assert!(!ChatTheme::emoji("x").is_default());

        // gift without the M.42 seam → the invalid bail
        let theme = ChatTheme::from_telegram(
            &mut env,
            telegram_api::ChatTheme::chatThemeUniqueGift(telegram_api::chatThemeUniqueGift {
                gift: None,
                theme_settings: vec![],
            }),
        );
        assert!(theme.is_default());
    }

    // --- the test promise helpers (the M.40 RecordingPromise convention) ---

    struct Recorded<T: Send + 'static>(std::sync::Arc<std::sync::Mutex<Option<T>>>);

    struct RecordingPromise<T: Send + 'static> {
        recorded: Recorded<T>,
    }

    impl<T: Send + 'static> PromiseInterface<T> for RecordingPromise<T> {
        fn set_value(&mut self, value: T) {
            *self.recorded.0.lock().unwrap() = Some(value);
        }

        fn set_error(&mut self, error: Status) {
            *self.recorded.0.lock().unwrap() = None;
            let _ = error;
        }
    }

    fn recording_promise<T: Send + 'static>() -> (Promise<T>, Recorded<T>) {
        let recorded = Recorded(std::sync::Arc::new(std::sync::Mutex::new(None)));
        (
            Promise::from_interface(RecordingPromise {
                recorded: Recorded(recorded.0.clone()),
            }),
            recorded,
        )
    }
}
