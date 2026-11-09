//! `td::StoryManager` (StoryManager.h/.cpp, M.47): the story state machine
//! — the `Story` row store, the active-stories/list machines, the stealth
//! mode pmc row, the server intakes (`on_get_story`/`on_get_stories`/
//! `on_get_dialog_stories`) and the request arms.
//!
//! Seams (the ThemeManagerEnv conventions):
//! - every server-bound query (`stories.getStoriesByID`,
//!   `stories.getAllStories`, ...) is a [`StoryManagerEnv`] seam: the
//!   net-drain 20 families (the §3.5 albums/metrics/search/live slice)
//!   dispatch through the `StoryEnv` net handle; the rest park at the
//!   `[blocked: net]` default seams — the query name is logged and the
//!   parked promise settles through the ported `on_*` answer half;
//! - the binlog log events (`DeleteStoryOnServer`, `ReadStoriesOnServer`,
//!   `LoadDialogExpiringStories`, `SendStory`, `EditStory`) are
//!   `[blocked: binlog]` — the base carries only the binlog pmc, so the
//!   restart replay (`on_binlog_events`) is unimplemented and the writers
//!   log the event they would have stored;
//! - the `Timeout`/`MultiTimeout` actor timers (`StoryReloadTimeout`,
//!   `StoryExpireTimeout`, `StoryCanGetViewersTimeout`, the stealth/interaction
//!   polls) are `[blocked: Timeout actor]`; the expiry *decisions* are
//!   ported and driven from `run_pending_timeouts` so the flows stay
//!   testable;
//! - the upload send path (`FileUploadId`/`UploadMediaCallback`/
//!   `do_send_story`) is `[blocked: FileUploader]` — `send_story` builds the
//!   local `Story` + `PendingStory` and parks it, reporting the not-sent
//!   state the C++ reports while the upload runs;
//! - MessagesManager/ChatManager user-story halves
//!   (`on_update_user_story_ids`, `update_story_max_reply_media_timestamp...`,
//!   `TopDialogManager`) are `[blocked]` seams on the env.
//!
//! Recorded divergences:
//! - `Story::store` keeps the C++ flag layout; the areas and the privacy
//!   rules bytes ride their own value planes here (see the module docs of
//!   [`MediaArea`]/[`UserPrivacySettingRules`]) and round-trip shape-identically.

use crate::dialog_id::{DialogId, DialogType};
use crate::files::{FileId, FileType};
use crate::message_entity::FormattedText;
use crate::notification::NotificationId;
use crate::photo::Photo;
use crate::reaction_type::ReactionType;
use crate::store_parse::{self, Parser};
use crate::story_content::{
    StoryContent, StoryContentType, can_edit_story_content, get_input_story_content,
    get_story_content, get_story_content_duration, get_story_content_file_ids,
    get_story_content_object, merge_story_contents,
};
use crate::story_db::{StoryDb, StoryDbGetActiveStoryListResult, StoryDbStory};
use crate::story_forward_info::StoryForwardInfo;
use crate::story_ids::{StoryAlbumId, StoryFullId, StoryId, StoryListId};
use crate::story_interaction_info::StoryInteractionInfo;
use crate::story_stealth_mode::StoryStealthMode;
use crate::td_api;
use crate::user_manager::UserId;
use std::collections::{BTreeSet, HashMap};
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::{Result, Status};

/// `MAX_SEARCH_STORIES` (StoryManager.h:455).
const MAX_SEARCH_STORIES: i32 = 100;
/// `OPENED_STORY_POLL_PERIOD` (StoryManager.h:457).
pub(crate) const OPENED_STORY_POLL_PERIOD: i32 = 60;
/// `VIEWED_STORY_POLL_PERIOD` (StoryManager.h:458).
const VIEWED_STORY_POLL_PERIOD: i32 = 300;
/// `DEFAULT_LOADED_EXPIRED_STORIES` (StoryManager.h:460).
const DEFAULT_LOADED_EXPIRED_STORIES: i32 = 50;

/// `td::DialogDate` (DialogDate.h) — the `(order, dialog_id)` pair the
/// story lists page by; `MIN_DIALOG_DATE`/`MAX_DIALOG_DATE` are the C++
/// `std::numeric_limits<int64>::min/max` orders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DialogDate {
    order: i64,
    dialog_id: DialogId,
}

impl DialogDate {
    pub const MIN_DIALOG_DATE_ORDER: i64 = i64::MIN;
    pub const MAX_DIALOG_DATE_ORDER: i64 = i64::MAX;

    pub fn new(order: i64, dialog_id: DialogId) -> Self {
        DialogDate { order, dialog_id }
    }

    pub fn get_order(self) -> i64 {
        self.order
    }

    pub fn get_dialog_id(self) -> DialogId {
        self.dialog_id
    }

    pub fn min_dialog_date() -> Self {
        DialogDate {
            order: Self::MIN_DIALOG_DATE_ORDER,
            dialog_id: DialogId::default(),
        }
    }

    pub fn max_dialog_date() -> Self {
        DialogDate {
            order: Self::MAX_DIALOG_DATE_ORDER,
            dialog_id: DialogId::default(),
        }
    }
}

impl PartialOrd for DialogDate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DialogDate {
    /// `operator<(DialogDate, DialogDate)` (DialogDate.h:54-56) — by order,
    /// then dialog id.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.order
            .cmp(&other.order)
            .then_with(|| self.dialog_id.get().cmp(&other.dialog_id.get()))
    }
}

/// The `td::MediaArea` story-plane stand-in ([blocked: MediaArea track]):
/// the C++ `MediaArea` binary row and its per-type validation live with the
/// media-area track; the story flows carry the already-converted
/// `td_api::StoryArea` the C++ `get_media_areas` produces and
/// `get_story_area_object` converts back to.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaArea {
    pub area: td_api::StoryArea,
}

impl MediaArea {
    /// `MediaArea::is_valid` — the position/type presence is the minimum
    /// validity the C++ constructor enforces.
    pub fn is_valid(&self) -> bool {
        self.area.position.is_some() && self.area.r#type.is_some()
    }

    /// `MediaArea::has_reaction_type` (MediaArea.cpp) — only the
    /// suggested-reaction area carries one.
    pub fn has_reaction_type(&self, reaction_type: &ReactionType) -> bool {
        match &self.area.r#type {
            Some(td_api::StoryAreaType::storyAreaTypeSuggestedReaction(area)) => {
                ReactionType::from_td_api(area.reaction_type.clone()) == *reaction_type
            }
            _ => false,
        }
    }
}

/// The `MediaArea(Td *, telegram_api::MediaArea)` intake
/// (MediaArea.cpp) — [blocked: MediaArea track]: the C++ constructor reads
/// the story media dimensions for the percentage conversion; until that
/// track lands every server-sent area is dropped here like the C++ drops
/// invalid ones.
fn media_area_from_telegram(media_area: telegram_api::MediaArea) -> Option<MediaArea> {
    let _ = media_area;
    tdutils::log_info!("MediaArea intake — [blocked: MediaArea track]");
    None
}

/// `MediaArea::get_media_areas` (MediaArea.cpp) — the input-areas
/// conversion; per-area validation beyond the position/type presence is
/// [blocked: MediaArea track].
pub fn get_media_areas(
    input_areas: Option<td_api::InputStoryAreas>,
    _old_areas: &[MediaArea],
) -> Vec<MediaArea> {
    let Some(input_areas) = input_areas else {
        return Vec::new();
    };
    input_areas
        .areas
        .into_iter()
        .map(|area| MediaArea {
            area: td_api::StoryArea {
                // 1.8.70: the shared `areaPosition` replaces
                // storyAreaPosition (same fields, both story areas and
                // TON wallet card stickers use it now).
                position: area.position.map(|position| td_api::AreaPosition {
                    x_percentage: position.x_percentage,
                    y_percentage: position.y_percentage,
                    width_percentage: position.width_percentage,
                    height_percentage: position.height_percentage,
                    rotation_angle: position.rotation_angle,
                    corner_radius_percentage: position.corner_radius_percentage,
                }),
                r#type: match area.r#type {
                    Some(td_api::InputStoryAreaType::inputStoryAreaTypeLocation(area)) => {
                        Some(td_api::StoryAreaType::storyAreaTypeLocation(
                            td_api::storyAreaTypeLocation {
                                location: area.location,
                                address: area.address,
                            },
                        ))
                    }
                    Some(td_api::InputStoryAreaType::inputStoryAreaTypePreviousVenue(area)) => {
                        // the previous-venue input resolves the Venue
                        // against the old areas (MediaArea.cpp:159-172) —
                        // [blocked: the venue track]; the interim area
                        // drops the venue payload. 1.8.70 removed
                        // inputStoryAreaTypeFoundVenue (the search-side
                        // twin) — only the previous-venue form remains.
                        let _ = (area.venue_provider, area.venue_id);
                        None
                    }
                    Some(td_api::InputStoryAreaType::inputStoryAreaTypeSuggestedReaction(area)) => {
                        Some(td_api::StoryAreaType::storyAreaTypeSuggestedReaction(
                            td_api::storyAreaTypeSuggestedReaction {
                                reaction_type: area.reaction_type,
                                total_count: 0,
                                is_dark: area.is_dark,
                                is_flipped: area.is_flipped,
                            },
                        ))
                    }
                    Some(td_api::InputStoryAreaType::inputStoryAreaTypeMessage(area)) => Some(
                        td_api::StoryAreaType::storyAreaTypeMessage(td_api::storyAreaTypeMessage {
                            chat_id: 0,
                            message_id: area.message_id,
                        }),
                    ),
                    Some(td_api::InputStoryAreaType::inputStoryAreaTypeLink(area)) => {
                        Some(td_api::StoryAreaType::storyAreaTypeLink(
                            td_api::storyAreaTypeLink { url: area.url },
                        ))
                    }
                    Some(td_api::InputStoryAreaType::inputStoryAreaTypeWeather(area)) => Some(
                        td_api::StoryAreaType::storyAreaTypeWeather(td_api::storyAreaTypeWeather {
                            emoji: area.emoji,
                            temperature: area.temperature,
                            background_color: 0,
                        }),
                    ),
                    Some(td_api::InputStoryAreaType::inputStoryAreaTypeUpgradedGift(area)) => {
                        Some(td_api::StoryAreaType::storyAreaTypeUpgradedGift(
                            td_api::storyAreaTypeUpgradedGift {
                                gift_name: area.gift_name,
                            },
                        ))
                    }
                    None => None,
                },
            },
        })
        .collect()
}

/// The story-plane subset of `td::UserPrivacySettingRule`
/// (UserPrivacySettingRule.h:80-93) — the story privacy constructors; the
/// full rule set (chat participants, bots, premium, the telegram
/// `PrivacyRule` intake) is the privacy track. The store bytes follow the
/// C++ `td::store(type_)` + the user-id vector (UserPrivacySettingRule.h).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserPrivacySettingRule {
    /// `UserPrivacySettingRule::Type` — the C++ discriminant order.
    r#type: i32,
    user_ids: Vec<UserId>,
}

impl UserPrivacySettingRule {
    const ALLOW_CONTACTS: i32 = 0;
    const ALLOW_CLOSE_FRIENDS: i32 = 1;
    const ALLOW_ALL: i32 = 2;
    const ALLOW_USERS: i32 = 3;
    const RESTRICT_USERS: i32 = 7;

    /// `UserPrivacySettingRule::get_input_privacy_rule`
    /// (UserPrivacySettingRule.cpp:211-240) — the story-plane subset.
    fn get_input_privacy_rule(
        &self,
        env: &mut impl StoryManagerEnv,
    ) -> telegram_api::InputPrivacyRule {
        match self.r#type {
            Self::ALLOW_CONTACTS => telegram_api::InputPrivacyRule::inputPrivacyValueAllowContacts(
                telegram_api::inputPrivacyValueAllowContacts,
            ),
            Self::ALLOW_CLOSE_FRIENDS => {
                telegram_api::InputPrivacyRule::inputPrivacyValueAllowCloseFriends(
                    telegram_api::inputPrivacyValueAllowCloseFriends,
                )
            }
            Self::ALLOW_ALL => telegram_api::InputPrivacyRule::inputPrivacyValueAllowAll(
                telegram_api::inputPrivacyValueAllowAll,
            ),
            Self::ALLOW_USERS => telegram_api::InputPrivacyRule::inputPrivacyValueAllowUsers(
                telegram_api::inputPrivacyValueAllowUsers {
                    users: env.get_input_users_force(&self.user_ids),
                },
            ),
            Self::RESTRICT_USERS => telegram_api::InputPrivacyRule::inputPrivacyValueDisallowUsers(
                telegram_api::inputPrivacyValueDisallowUsers {
                    users: env.get_input_users_force(&self.user_ids),
                },
            ),
            _ => unreachable!("the story rule types"),
        }
    }

    fn allow_all() -> Self {
        UserPrivacySettingRule {
            r#type: Self::ALLOW_ALL,
            user_ids: Vec::new(),
        }
    }

    fn allow_contacts() -> Self {
        UserPrivacySettingRule {
            r#type: Self::ALLOW_CONTACTS,
            user_ids: Vec::new(),
        }
    }

    fn allow_close_friends() -> Self {
        UserPrivacySettingRule {
            r#type: Self::ALLOW_CLOSE_FRIENDS,
            user_ids: Vec::new(),
        }
    }

    fn allow_users(user_ids: Vec<UserId>) -> Self {
        UserPrivacySettingRule {
            r#type: Self::ALLOW_USERS,
            user_ids,
        }
    }

    fn restrict_users(user_ids: Vec<UserId>) -> Self {
        UserPrivacySettingRule {
            r#type: Self::RESTRICT_USERS,
            user_ids,
        }
    }

    /// `UserPrivacySettingRule::store` (UserPrivacySettingRule.h).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.r#type);
        if self.r#type == Self::ALLOW_USERS || self.r#type == Self::RESTRICT_USERS {
            store_parse::store_i32(out, self.user_ids.len() as i32);
            for user_id in &self.user_ids {
                store_parse::store_i64(out, user_id.get());
            }
        }
    }

    /// `UserPrivacySettingRule::parse` (UserPrivacySettingRule.h).
    pub fn parse(parser: &mut Parser) -> std::result::Result<UserPrivacySettingRule, &'static str> {
        let r#type = parser.i32_()?;
        let mut user_ids = Vec::new();
        if r#type == Self::ALLOW_USERS || r#type == Self::RESTRICT_USERS {
            let size = parser.i32_()?;
            for _ in 0..size {
                let user_id = UserId::new(parser.i64_()?);
                if !user_id.is_valid() {
                    return Err("Failed to parse user identifiers");
                }
                user_ids.push(user_id);
            }
        } else if !(0..=11).contains(&r#type)
            || !matches!(r#type, 4 | 8 | 9 | 10 | 11) && r#type > Self::RESTRICT_USERS
        {
            return Err("Invalid privacy rule type");
        }
        Ok(UserPrivacySettingRule { r#type, user_ids })
    }
}

/// The story-plane `td::UserPrivacySettingRules`
/// (UserPrivacySettingRule.h:109-155): the rule list the stories persist
/// and the story privacy-settings conversion of
/// UserPrivacySettingRule.cpp:312-357 / 359-390.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserPrivacySettingRules {
    rules: Vec<UserPrivacySettingRule>,
}

impl UserPrivacySettingRules {
    /// `UserPrivacySettingRules::get_user_privacy_setting_rules(td_api::StoryPrivacySettings)`
    /// (UserPrivacySettingRule.cpp:312-357).
    pub fn from_story_privacy_settings(
        settings: Option<td_api::StoryPrivacySettings>,
    ) -> Result<UserPrivacySettingRules> {
        let Some(settings) = settings else {
            return Result::Err(Status::error(400, "StoryPrivacySettings must be non-empty"));
        };
        let mut result = UserPrivacySettingRules::default();
        match settings {
            td_api::StoryPrivacySettings::storyPrivacySettingsEveryone(settings) => {
                if !settings.except_user_ids.is_empty() {
                    result.rules.push(UserPrivacySettingRule::restrict_users(
                        settings
                            .except_user_ids
                            .iter()
                            .map(|&id| UserId::new(id))
                            .collect(),
                    ));
                }
                result.rules.push(UserPrivacySettingRule::allow_all());
            }
            td_api::StoryPrivacySettings::storyPrivacySettingsContacts(settings) => {
                if !settings.except_user_ids.is_empty() {
                    result.rules.push(UserPrivacySettingRule::restrict_users(
                        settings
                            .except_user_ids
                            .iter()
                            .map(|&id| UserId::new(id))
                            .collect(),
                    ));
                }
                result.rules.push(UserPrivacySettingRule::allow_contacts());
            }
            td_api::StoryPrivacySettings::storyPrivacySettingsCloseFriends(_) => {
                result
                    .rules
                    .push(UserPrivacySettingRule::allow_close_friends());
            }
            td_api::StoryPrivacySettings::storyPrivacySettingsSelectedUsers(settings) => {
                result.rules.push(UserPrivacySettingRule::allow_users(
                    settings
                        .user_ids
                        .iter()
                        .map(|&id| UserId::new(id))
                        .collect(),
                ));
            }
        }
        Result::Ok(result)
    }

    /// `UserPrivacySettingRules::get_input_privacy_rules`
    /// (UserPrivacySettingRule.cpp:389-396) — the story-plane form over
    /// the five story rule types; the trailing
    /// `inputPrivacyValueDisallowAll` drops.
    pub fn get_input_privacy_rules(
        &self,
        env: &mut impl StoryManagerEnv,
    ) -> Vec<telegram_api::InputPrivacyRule> {
        let mut result = self
            .rules
            .iter()
            .map(|rule| rule.get_input_privacy_rule(env))
            .collect::<Vec<_>>();
        if result.last().is_some_and(|rule| {
            matches!(
                rule,
                telegram_api::InputPrivacyRule::inputPrivacyValueDisallowAll(_)
            )
        }) {
            result.pop();
        }
        result
    }

    /// `UserPrivacySettingRules::get_story_privacy_settings_object`
    /// (UserPrivacySettingRule.cpp:359-390); `None` for the C++ nullptr.
    pub fn get_story_privacy_settings_object(
        &self,
        env: &mut impl StoryManagerEnv,
    ) -> Option<td_api::StoryPrivacySettings> {
        fn get_ids(env: &mut impl StoryManagerEnv, user_ids: &[UserId]) -> Vec<i64> {
            env.get_user_ids_object(user_ids, "storyPrivacySettings")
        }
        match self.rules.len() {
            1 => match self.rules[0].r#type {
                UserPrivacySettingRule::ALLOW_ALL => {
                    Some(td_api::StoryPrivacySettings::storyPrivacySettingsEveryone(
                        td_api::storyPrivacySettingsEveryone::default(),
                    ))
                }
                UserPrivacySettingRule::ALLOW_CONTACTS => {
                    Some(td_api::StoryPrivacySettings::storyPrivacySettingsContacts(
                        td_api::storyPrivacySettingsContacts::default(),
                    ))
                }
                UserPrivacySettingRule::ALLOW_CLOSE_FRIENDS => Some(
                    td_api::StoryPrivacySettings::storyPrivacySettingsCloseFriends(
                        td_api::storyPrivacySettingsCloseFriends,
                    ),
                ),
                UserPrivacySettingRule::ALLOW_USERS => Some(
                    td_api::StoryPrivacySettings::storyPrivacySettingsSelectedUsers(
                        td_api::storyPrivacySettingsSelectedUsers {
                            user_ids: get_ids(env, &self.rules[0].user_ids),
                        },
                    ),
                ),
                _ => Some(
                    td_api::StoryPrivacySettings::storyPrivacySettingsSelectedUsers(
                        td_api::storyPrivacySettingsSelectedUsers::default(),
                    ),
                ),
            },
            2 if self.rules[0].r#type == UserPrivacySettingRule::RESTRICT_USERS
                && self.rules[1].r#type == UserPrivacySettingRule::ALLOW_ALL =>
            {
                Some(td_api::StoryPrivacySettings::storyPrivacySettingsEveryone(
                    td_api::storyPrivacySettingsEveryone {
                        except_user_ids: get_ids(env, &self.rules[0].user_ids),
                    },
                ))
            }
            2 if self.rules[0].r#type == UserPrivacySettingRule::RESTRICT_USERS
                && self.rules[1].r#type == UserPrivacySettingRule::ALLOW_CONTACTS =>
            {
                Some(td_api::StoryPrivacySettings::storyPrivacySettingsContacts(
                    td_api::storyPrivacySettingsContacts {
                        except_user_ids: get_ids(env, &self.rules[0].user_ids),
                    },
                ))
            }
            _ => Some(
                td_api::StoryPrivacySettings::storyPrivacySettingsSelectedUsers(
                    td_api::storyPrivacySettingsSelectedUsers::default(),
                ),
            ),
        }
    }

    /// `UserPrivacySettingRules::store` (UserPrivacySettingRule.h:144-146).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.rules.len() as i32);
        for rule in &self.rules {
            rule.store(out);
        }
    }

    /// `UserPrivacySettingRules::parse` (UserPrivacySettingRule.h:148-155).
    pub fn parse(
        parser: &mut Parser,
    ) -> std::result::Result<UserPrivacySettingRules, &'static str> {
        let size = parser.i32_()?;
        let mut rules = Vec::with_capacity(size as usize);
        for _ in 0..size {
            rules.push(UserPrivacySettingRule::parse(parser)?);
        }
        Ok(UserPrivacySettingRules { rules })
    }
}

/// `StoryManager::Story` (StoryManager.h:61-91).
#[derive(Clone, Debug, Default)]
pub struct Story {
    sender_dialog_id: DialogId,
    date: i32,
    expire_date: i32,
    receive_date: i32,
    is_edited: bool,
    is_pinned: bool,
    is_public: bool,
    is_for_close_friends: bool,
    is_for_contacts: bool,
    is_for_selected_contacts: bool,
    is_live: bool,
    is_outgoing: bool,
    noforwards: bool,
    /// `is_update_sent_` — whether the story is known to the app.
    is_update_sent: bool,
    forward_info: Option<StoryForwardInfo>,
    interaction_info: StoryInteractionInfo,
    chosen_reaction_type: ReactionType,
    privacy_rules: UserPrivacySettingRules,
    content: Option<StoryContent>,
    /// The story areas; the C++ `vector<MediaArea>` over its own byte plane
    /// ([blocked: MediaArea track]).
    areas: Vec<MediaArea>,
    album_ids: Vec<StoryAlbumId>,
    caption: FormattedText,
    global_id: i64,
}

impl PartialEq for Story {
    /// `operator==(Story)` is memberwise in the places the C++ compares
    /// (the forward-info comparison, StoryManager.cpp:4496); the port
    /// compares the full payload.
    fn eq(&self, other: &Self) -> bool {
        self.sender_dialog_id == other.sender_dialog_id
            && self.date == other.date
            && self.expire_date == other.expire_date
            && self.is_edited == other.is_edited
            && self.is_pinned == other.is_pinned
            && self.is_public == other.is_public
            && self.is_for_close_friends == other.is_for_close_friends
            && self.is_for_contacts == other.is_for_contacts
            && self.is_for_selected_contacts == other.is_for_selected_contacts
            && self.is_live == other.is_live
            && self.is_outgoing == other.is_outgoing
            && self.noforwards == other.noforwards
            && self.forward_info == other.forward_info
            && self.interaction_info == other.interaction_info
            && self.chosen_reaction_type == other.chosen_reaction_type
            && self.privacy_rules == other.privacy_rules
            && self.content == other.content
            && self.areas == other.areas
            && self.album_ids == other.album_ids
            && self.caption == other.caption
    }
}

impl Story {
    /// `StoryManager::Story::store` (StoryManager.cpp:1748-1813).
    pub fn store(&self, env: &mut impl StoryManagerEnv, out: &mut Vec<u8>) {
        let has_receive_date = self.receive_date != 0;
        let has_interaction_info = !self.interaction_info.is_empty();
        let has_privacy_rules = self.privacy_rules != UserPrivacySettingRules::default();
        let has_content = self.content.is_some();
        let has_caption = !self.caption.text.is_empty();
        let has_areas = !self.areas.is_empty();
        let has_chosen_reaction_type = !self.chosen_reaction_type.is_empty();
        let has_forward_info = self.forward_info.is_some();
        let has_sender_dialog_id = self.sender_dialog_id != DialogId::default();
        let has_album_ids = !self.album_ids.is_empty();
        store_parse::store_flags(
            out,
            [
                self.is_edited,
                self.is_pinned,
                self.is_public,
                self.is_for_close_friends,
                self.noforwards,
                has_receive_date,
                has_interaction_info,
                has_privacy_rules,
                has_content,
                has_caption,
                self.is_for_contacts,
                self.is_for_selected_contacts,
                has_areas,
                has_chosen_reaction_type,
                self.is_outgoing,
                has_forward_info,
                has_sender_dialog_id,
                has_album_ids,
                self.is_live,
            ],
        );
        store_parse::store_i32(out, self.date);
        store_parse::store_i32(out, self.expire_date);
        if has_receive_date {
            store_parse::store_i32(out, self.receive_date);
        }
        if has_interaction_info {
            self.interaction_info.store(out);
        }
        if has_privacy_rules {
            self.privacy_rules.store(out);
        }
        if has_content {
            self.content
                .as_ref()
                .expect("checked above")
                .store(env, out);
        }
        if has_caption {
            self.caption.store(out);
        }
        if has_areas {
            // `td::store(areas_, storer)` — [blocked: MediaArea track]; the
            // interim row stores the count and the td_api payload.
            store_parse::store_i32(out, self.areas.len() as i32);
            for area in &self.areas {
                area.area.store_boxed(out);
            }
        }
        if has_chosen_reaction_type {
            self.chosen_reaction_type.store(out);
        }
        if has_forward_info {
            self.forward_info
                .as_ref()
                .expect("checked above")
                .store(out);
        }
        if has_sender_dialog_id {
            self.sender_dialog_id.store(out);
        }
        if has_album_ids {
            store_parse::store_i32(out, self.album_ids.len() as i32);
            for album_id in &self.album_ids {
                album_id.store(out);
            }
        }
    }

    /// `StoryManager::Story::parse` (StoryManager.cpp:1816-1881).
    pub fn parse(
        env: &mut impl StoryManagerEnv,
        parser: &mut Parser,
    ) -> std::result::Result<Story, &'static str> {
        let mut story = Story::default();
        let [
            is_edited,
            is_pinned,
            is_public,
            is_for_close_friends,
            noforwards,
            has_receive_date,
            has_interaction_info,
            has_privacy_rules,
            has_content,
            has_caption,
            is_for_contacts,
            is_for_selected_contacts,
            has_areas,
            has_chosen_reaction_type,
            is_outgoing,
            has_forward_info,
            has_sender_dialog_id,
            has_album_ids,
            is_live,
        ] = parser.parse_flags()?;
        story.is_edited = is_edited;
        story.is_pinned = is_pinned;
        story.is_public = is_public;
        story.is_for_close_friends = is_for_close_friends;
        story.noforwards = noforwards;
        story.is_for_contacts = is_for_contacts;
        story.is_for_selected_contacts = is_for_selected_contacts;
        story.is_outgoing = is_outgoing;
        story.is_live = is_live;
        story.date = parser.i32_()?;
        story.expire_date = parser.i32_()?;
        if has_receive_date {
            story.receive_date = parser.i32_()?;
        }
        if has_interaction_info {
            story.interaction_info = StoryInteractionInfo::parse(parser)?;
        }
        if has_privacy_rules {
            story.privacy_rules = UserPrivacySettingRules::parse(parser)?;
        }
        if has_content {
            story.content = Some(StoryContent::parse(env, parser)?);
        }
        if has_caption {
            story.caption = FormattedText::parse(parser)?;
        }
        if has_areas {
            let size = parser.i32_()?;
            for _ in 0..size {
                let json = parser.string_bytes()?;
                let area = match td_api::StoryArea::parse_bare(&json) {
                    Ok(value) => value,
                    Err(_) => return Err("Failed to parse story area"),
                };
                story.areas.push(MediaArea { area });
            }
        }
        if has_chosen_reaction_type {
            story.chosen_reaction_type = ReactionType::parse(parser)?;
        }
        if has_forward_info {
            story.forward_info = Some(StoryForwardInfo::parse(parser)?);
        }
        if has_sender_dialog_id {
            story.sender_dialog_id = DialogId::parse_from(parser)?;
        }
        if has_album_ids {
            let size = parser.i32_()?;
            for _ in 0..size {
                story.album_ids.push(StoryAlbumId::parse(parser)?);
            }
        }
        Ok(story)
    }
}

/// `StoryManager::StoryInfo` (StoryManager.h:93-105).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoryInfo {
    pub story_id: StoryId,
    pub date: i32,
    pub expire_date: i32,
    pub is_for_close_friends: bool,
    pub is_live: bool,
}

impl StoryInfo {
    /// `StoryManager::StoryInfo::store` (StoryManager.cpp:1884-1893).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags(out, [self.is_for_close_friends, self.is_live]);
        self.story_id.store(out);
        store_parse::store_i32(out, self.date);
        store_parse::store_i32(out, self.expire_date);
    }

    /// `StoryManager::StoryInfo::parse` (StoryManager.cpp:1896-1905).
    pub fn parse(parser: &mut Parser) -> std::result::Result<StoryInfo, &'static str> {
        let [is_for_close_friends, is_live] = parser.parse_flags()?;
        let mut result = StoryInfo {
            is_for_close_friends,
            is_live,
            ..StoryInfo::default()
        };
        result.story_id = StoryId::parse(parser)?;
        result.date = parser.i32_()?;
        result.expire_date = parser.i32_()?;
        Ok(result)
    }
}

/// `StoryManager::SavedActiveStories` (StoryManager.h:170-179).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SavedActiveStories {
    pub max_read_story_id: StoryId,
    pub story_infos: Vec<StoryInfo>,
}

impl SavedActiveStories {
    /// `StoryManager::SavedActiveStories::store` (StoryManager.cpp:1953-1964).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_max_read_story_id = self.max_read_story_id.is_valid();
        store_parse::store_flags(out, [has_max_read_story_id]);
        store_parse::store_i32(out, self.story_infos.len() as i32);
        for story_info in &self.story_infos {
            story_info.store(out);
        }
        if has_max_read_story_id {
            self.max_read_story_id.store(out);
        }
    }

    /// `StoryManager::SavedActiveStories::parse` (StoryManager.cpp:1967-1977).
    pub fn parse(parser: &mut Parser) -> std::result::Result<SavedActiveStories, &'static str> {
        let [has_max_read_story_id] = parser.parse_flags()?;
        let mut result = SavedActiveStories::default();
        let size = parser.i32_()?;
        for _ in 0..size {
            result.story_infos.push(StoryInfo::parse(parser)?);
        }
        if has_max_read_story_id {
            result.max_read_story_id = StoryId::parse(parser)?;
        }
        Ok(result)
    }
}

/// `StoryManager::SavedStoryList` (StoryManager.h:199-209).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SavedStoryList {
    pub state: String,
    pub total_count: i32,
    pub has_more: bool,
}

impl SavedStoryList {
    /// `StoryManager::SavedStoryList::store` (StoryManager.cpp:1980-1987).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags(out, [self.has_more]);
        store_parse::store_string(out, &self.state);
        store_parse::store_i32(out, self.total_count);
    }

    /// `StoryManager::SavedStoryList::parse` (StoryManager.cpp:1990-1997).
    pub fn parse(parser: &mut Parser) -> std::result::Result<SavedStoryList, &'static str> {
        let [has_more] = parser.parse_flags()?;
        let mut result = SavedStoryList {
            has_more,
            ..SavedStoryList::default()
        };
        result.state = parser.string()?;
        result.total_count = parser.i32_()?;
        Ok(result)
    }
}

/// `StoryManager::ActiveStories` (StoryManager.h:161-168).
#[derive(Clone, Debug, Default)]
pub struct ActiveStories {
    pub max_read_story_id: StoryId,
    pub story_ids: Vec<StoryId>,
    pub story_list_id: StoryListId,
    pub private_order: i64,
    pub public_order: i64,
    pub can_be_archived: bool,
}

/// `StoryManager::StoryList` (StoryManager.h:181-197); the promise queues
/// and the pending-query ladders ride the net seams.
#[derive(Clone, Debug, Default)]
pub struct StoryList {
    pub server_total_count: i32,
    pub sent_total_count: i32,
    pub state: String,
    pub is_reloaded_server_total_count: bool,
    pub server_has_more: bool,
    pub database_has_more: bool,
    /// `ordered_stories_` — all known active stories from the story list.
    pub ordered_stories: BTreeSet<DialogDate>,
    /// `last_loaded_database_dialog_date_`.
    pub last_loaded_database_dialog_date: DialogDate,
    /// `list_last_story_date_`.
    pub list_last_story_date: DialogDate,
}

impl Default for DialogDate {
    fn default() -> Self {
        DialogDate::min_dialog_date()
    }
}

impl StoryList {
    /// `StoryList()` (StoryList.h) — the caller planes land with the
    /// database reload work ([blocked: Td update sends]).
    #[allow(dead_code)]
    fn new() -> Self {
        StoryList {
            server_total_count: -1,
            sent_total_count: -1,
            state: String::new(),
            is_reloaded_server_total_count: false,
            server_has_more: true,
            database_has_more: false,
            ordered_stories: BTreeSet::new(),
            last_loaded_database_dialog_date: DialogDate::min_dialog_date(),
            list_last_story_date: DialogDate::min_dialog_date(),
        }
    }
}

/// `StoryManager::PendingStoryViews` (StoryManager.h:156-159).
#[derive(Clone, Debug, Default)]
pub struct PendingStoryViews {
    pub story_ids: BTreeSet<i32>,
    pub has_query: bool,
}

/// The `StoryManager::BeingEditedStory` (StoryManager.h:107-115).
#[derive(Default)]
struct BeingEditedStory {
    content: Option<StoryContent>,
    areas: Vec<MediaArea>,
    caption: FormattedText,
    edit_media_areas: bool,
    edit_caption: bool,
    promises: Vec<Promise<()>>,
    log_event_id: u64,
}

/// The StoryManager in-flight query kinds — the C++ ResultHandler
/// classes of the story plane: the story-core sends and reads
/// (StoryManager.cpp:69-1748, the §3.5 core families) and the
/// §3.5 net-drain 20 slice (the albums, the interaction metrics, the
/// pinned/archive reads, the hashtag search, the live/business sends
/// and the send-right checks). The kinds carry the Copy handler members
/// the answer halves read (the dialog ids for the `on_get_dialog_error`
/// heads, the toggle flags); the non-Copy handler state (the promises,
/// the pending stories, the load snapshots) parks on the manager next to
/// them — the §5 FIFO-per-kind convention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StoryNetKind {
    /// `GetAllStoriesQuery` (StoryManager.cpp:69) — the list id; the
    /// `is_next`/old-state promise-lambda captures park in
    /// [`StoryManager::in_flight_load_list`].
    GetAllStories { story_list_id: StoryListId },
    /// `ToggleStoriesHiddenQuery` (StoryManager.cpp:102).
    ToggleStoriesHidden {
        dialog_id: DialogId,
        are_hidden: bool,
    },
    /// `GetAllReadPeerStoriesQuery` (StoryManager.cpp:140) — the
    /// promise-less fire-and-forget (the answer rides the updates
    /// engine with an empty promise).
    GetAllReadPeerStories,
    /// `ToggleAllStoriesHiddenQuery` (StoryManager.cpp:162) — the flag
    /// is the `on_synchronized_archive_all_stories` lambda capture.
    ToggleAllStoriesHidden { set_archive_all_stories: bool },
    /// `IncrementStoryViewsQuery` (StoryManager.cpp:190).
    IncrementStoryViews { dialog_id: DialogId },
    /// `ReadStoriesQuery` (StoryManager.cpp:224).
    ReadStories { dialog_id: DialogId },
    /// `SendStoryReactionQuery` (StoryManager.cpp:257).
    SendStoryReaction { story_full_id: StoryFullId },
    /// `GetPeerStoriesQuery` (StoryManager.cpp:504).
    GetPeerStories { dialog_id: DialogId },
    /// `EditStoryPrivacyQuery` (StoryManager.cpp:609).
    EditStoryPrivacy { dialog_id: DialogId },
    /// `ToggleStoryPinnedQuery` (StoryManager.cpp:653) — the C++ answer
    /// settles without reading the Updates body; the lambda captures
    /// ride for the `on_toggle_story_is_pinned` tail.
    ToggleStoryPinned {
        story_full_id: StoryFullId,
        is_pinned: bool,
    },
    /// `DeleteStoriesQuery` (StoryManager.cpp:689).
    DeleteStories { dialog_id: DialogId },
    /// `TogglePinnedStoriesToTopQuery` (StoryManager.cpp:824).
    TogglePinnedStoriesToTop { dialog_id: DialogId },
    /// `ReportStoryQuery` (StoryManager.cpp:890).
    ReportStory { dialog_id: DialogId },
    /// `ActivateStealthModeQuery` (StoryManager.cpp:1225).
    ActivateStealthMode,
    /// `StoryManager::SendStoryQuery` (StoryManager.cpp:1334) — the
    /// `delete_pending_story` tail parks in
    /// [`StoryManager::pending_send_story_tail`]; the send leg itself
    /// stays StoryContent-gated (see the seam note) — the kind rides the
    /// injected-answer tests until that plane lands.
    #[allow(dead_code)] // constructed at the StoryContent-plane landing
    SendStory { dialog_id: DialogId },
    /// `StoryManager::RepostBusinessStoryQuery` (StoryManager.cpp:1439)
    /// — `stories.sendStory` site two (the repost).
    RepostBusinessStory { dialog_id: DialogId },
    /// `StoryManager::EditStoryQuery` (StoryManager.cpp:1571) — the
    /// handler's `random_id_` capture is the edit generation (the
    /// `delete_pending_story` guard drops the stale edits).
    EditStory {
        story_full_id: StoryFullId,
        generation: i64,
    },
    /// `GetStoryViewsListQuery` (StoryManager.cpp:303-341) — the
    /// `getStoryInteractions` wire leg; the kind carries the story for
    /// the `on_get_story_interactions` intake flags (the C++
    /// query_promise lambda captures), the dialog for the error tail
    /// (the handler's `dialog_id_` member).
    GetStoryViewsList {
        story_full_id: StoryFullId,
        is_full: bool,
        is_first: bool,
    },
    /// `GetStoryReactionsListQuery` (StoryManager.cpp:346-391) — the
    /// `getChatStoryInteractions` wire leg; the story rides the kind
    /// (the `on_get_dialog_story_interactions` capture), the dialog for
    /// the error tail.
    GetStoryReactionsList { story_full_id: StoryFullId },
    /// `GetStoriesByIDQuery` (StoryManager.cpp:394-430) — the reload
    /// chain's single-story fetch; the promise parks in
    /// `reload_story_queries` (an exact keying), the handler's
    /// `story_ids_`/`dialog_id_` collapse onto the one story full id.
    GetStoriesByID { story_full_id: StoryFullId },
    /// `GetPinnedStoriesQuery` (StoryManager.cpp:432-466) — the
    /// `getChatPostedToChatPageStories` leg; the promises park in
    /// `pending_pinned_stories` keyed by the dialog.
    GetPinnedStories { dialog_id: DialogId },
    /// `GetStoriesArchiveQuery` (StoryManager.cpp:468-502) — the
    /// `getChatArchivedStories` leg; the promises park in
    /// `pending_archive_stories` keyed by the dialog.
    GetStoriesArchive { dialog_id: DialogId },
    /// `GetStoriesViewsQuery` (StoryManager.cpp:857-885) — the
    /// interaction-count poll (no promise); the handler's
    /// `dialog_id_`/`story_ids_` members park in the
    /// `pending_get_stories_views` context queue.
    GetStoriesViews,
    /// `SearchStoriesQuery` (StoryManager.cpp:724-815) — the three
    /// `searchPublicStories*` arms share the query; the kind carries
    /// the arm's dialog for the error tail (the handler's `dialog_id_`,
    /// default for the area arms), the promises park in
    /// `pending_search_stories`.
    SearchStories { dialog_id: DialogId },
    /// `GetStoryAlbumsQuery` (StoryManager.cpp:950-997) — the
    /// `getChatStoryAlbums` leg; the promises park in
    /// `pending_story_albums` keyed by the owner.
    GetStoryAlbums { owner_dialog_id: DialogId },
    /// `GetAlbumStoriesQuery` (StoryManager.cpp:999-1031) — the
    /// `getStoryAlbumStories` leg; the promises park in
    /// `pending_album_stories` keyed by the (dialog, album) pair.
    GetAlbumStories {
        dialog_id: DialogId,
        story_album_id: StoryAlbumId,
    },
    /// `CreateStoryAlbumQuery` (StoryManager.cpp:1033-1072) — the
    /// promise parks in the `pending_create_story_albums` FIFO with the
    /// handler's `story_ids_` member (the
    /// `update_story_albums` refire reads it at the answer).
    CreateStoryAlbum { owner_dialog_id: DialogId },
    /// `ReorderStoryAlbumsQuery` (StoryManager.cpp:1074-1092) — the
    /// promise parks in `pending_unit_queries`.
    ReorderStoryAlbums { owner_dialog_id: DialogId },
    /// `DeleteStoryAlbumQuery` (StoryManager.cpp:1107-1131) — the
    /// promise parks in `pending_unit_queries`.
    DeleteStoryAlbum { owner_dialog_id: DialogId },
    /// `UpdateStoryAlbumQuery` (StoryManager.cpp:1138-1240) — the four
    /// `setStoryAlbumName`/`add`/`remove`/`reorder` album-stories arms
    /// share the query; the promise parks in
    /// `pending_update_story_albums` with the handler's rollback
    /// members (`story_album_id_`, `deleted_story_ids_`,
    /// `added_story_ids_`).
    UpdateStoryAlbum,
    /// `StartLiveStoryQuery` (StoryManager.cpp:1506-1569) — the
    /// `startLiveStory` leg; the promise parks in
    /// `pending_start_live_story`.
    StartLiveStory { dialog_id: DialogId },
    /// `CanSendStoryQuery` (StoryManager.cpp:1296-1334) — the
    /// `canPostStory` leg (the paid-reaction count remains answer); the
    /// promise parks in `pending_can_post_story`.
    CanSendStory { dialog_id: DialogId },
    /// `GetChatsToSendStoriesQuery` (StoryManager.cpp:1253-1294) — the
    /// `getChatsToPostStories` leg; the promise parks in
    /// `pending_dialogs_to_send_stories`.
    GetChatsToSendStories,
    /// `StoryManager::EditBusinessStoryQuery` (StoryManager.cpp:1656-1717)
    /// — the bot business edit; the `BeingEditedBusinessStory` (the
    /// content/areas/caption/privacy rules + the promise — the C++
    /// `being_edited_business_stories_` row) parks keyed by the kind's
    /// `send_story_num`.
    EditBusinessStory { send_story_num: u32 },
}

/// The post-`on_get_updates` continuations of the story-core answer
/// halves — the C++ `PromiseCreator::lambda` chains the handlers pass
/// to the updates engine (the engine settles the promise, the lambda
/// runs the manager intake).
pub(crate) enum StoryNetTail {
    /// `SendStoryReactionQuery`'s lambda → `on_set_story_reaction`
    /// (StoryManager.cpp:3485-3490) — the caller promises pop from
    /// [`StoryManager::pending_story_reactions`].
    SetStoryReaction { story_full_id: StoryFullId },
    /// `SendStoryQuery`'s lambda → `delete_pending_story`
    /// (StoryManager.cpp:1400-1408) — the pending story pops from
    /// [`StoryManager::pending_send_story_tail`]. (The
    /// `RepostBusinessStoryQuery` answer runs inline — the C++ processes
    /// the users/chats and `extract_story` without the updates engine,
    /// cpp:1472-1488.)
    SendStory,
    /// `EditStoryQuery`'s lambda → `delete_pending_story`'s edit arm
    /// (StoryManager.cpp:1598-1607) — the ported `on_edit_story`.
    EditStory { story_full_id: StoryFullId },
}

/// `StoryManager::BeingEditedBusinessStory` (StoryManager.h:117-124) —
/// the bot business edit's parked row; keyed by the send story num like
/// the C++ `being_edited_business_stories_`. The content/areas/caption/
/// privacy fields are the C++ members the FILE_PART re-fire
/// (`on_send_story_file_parts_missing` → `do_send_story`) re-sends —
/// they read when the FileUploader plane lands.
#[allow(dead_code)] // the re-upload re-fire rides the FileUploader plane
struct BeingEditedBusinessStory {
    dialog_id: DialogId,
    story_id: StoryId,
    content: StoryContent,
    areas: Vec<MediaArea>,
    caption: FormattedText,
    privacy_rules: UserPrivacySettingRules,
    promise: Promise<Option<td_api::Story>>,
}

/// The `G()`/`td_` surface of the manager. The net queries are the
/// `[blocked: net]` query seams; the `send_update` outbox is drained by the
/// Td loop.
pub trait StoryManagerEnv {
    /// `td_->auth_manager_->is_authorized()`.
    fn is_authorized(&self) -> bool;

    /// `td_->auth_manager_->is_bot()`.
    fn is_bot(&self) -> bool;

    /// `G()->use_message_database()`.
    fn use_message_database(&self) -> bool;

    /// `G()->unix_time()`.
    fn unix_time(&mut self) -> i32;

    /// `G()->td_db()->get_binlog_pmc()->get`.
    fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;

    /// `G()->td_db()->get_binlog_pmc()->set`.
    fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]);

    /// `G()->td_db()->get_binlog_pmc()->erase`.
    fn binlog_pmc_erase(&mut self, key: &[u8]);

    /// `td_->option_manager_->get_option_integer(name)` with the C++
    /// default.
    fn get_option_integer(&mut self, name: &str, default_value: i64) -> i64;

    /// `td_->option_manager_->get_option_boolean(name)`.
    fn get_option_boolean(&mut self, name: &str) -> bool;

    /// `td_->option_manager_->set_option_empty(name)`.
    fn set_option_empty(&mut self, name: &str);

    /// `send_closure(G()->td(), &Td::send_update, ...)`.
    fn send_update(&mut self, update: td_api::Update);

    /// `td_->user_manager_->on_get_users(users, source)`.
    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str);

    /// `td_->chat_manager_->on_get_chats(chats, source)`.
    fn on_get_chats(&mut self, chats: Vec<telegram_api::Chat>, source: &str);

    /// `td_->dialog_manager_->force_create_dialog`.
    fn force_create_dialog(&mut self, dialog_id: DialogId, source: &str, force: bool);

    /// `td_->dialog_manager_->check_dialog_access`.
    fn check_dialog_access(
        &mut self,
        dialog_id: DialogId,
        allow_secret_chats: bool,
        need_write_access: bool,
        source: &str,
    ) -> Result<()>;

    /// `td_->dialog_manager_->have_dialog_force`.
    fn have_dialog_force(&mut self, dialog_id: DialogId, source: &str) -> bool;

    /// `td_->dialog_manager_->have_input_peer`.
    fn have_input_peer(
        &mut self,
        dialog_id: DialogId,
        allow_secret_chats: bool,
        need_write: bool,
    ) -> bool;

    /// `td_->dialog_manager_->get_my_dialog_id`.
    fn get_my_dialog_id(&self) -> DialogId;

    /// `td_->dialog_manager_->get_chat_id_object`.
    fn get_chat_id_object(&mut self, dialog_id: DialogId, source: &str) -> i64;

    /// `td_->dialog_manager_->is_anonymous_administrator` —
    /// [blocked: M.6 dialog admin surface]; the interim answer is `false`
    /// (the stub inventory answers the same).
    fn is_anonymous_administrator(&mut self, dialog_id: DialogId) -> bool {
        let _ = dialog_id;
        tdutils::log_info!("is_anonymous_administrator — [blocked: M.6 DialogManager]");
        false
    }

    /// `td_->dialog_manager_->is_group_dialog`.
    fn is_group_dialog(&mut self, dialog_id: DialogId) -> bool;

    /// `td_->user_manager_->get_my_id`.
    fn get_my_id(&self) -> UserId;

    /// `td_->user_manager_->have_min_user`.
    fn have_min_user(&self, user_id: UserId) -> bool;

    /// `td_->user_manager_->get_user_ids_object`.
    fn get_user_ids_object(&mut self, user_ids: &[UserId], source: &str) -> Vec<i64>;

    /// `td_->user_manager_->is_user_bot`.
    fn is_user_bot(&self, user_id: UserId) -> bool;

    /// `td_->user_manager_->is_user_premium`.
    fn is_user_premium(&self, user_id: UserId) -> bool;

    /// `td_->user_manager_->is_user_contact`.
    fn is_user_contact(&self, user_id: UserId) -> bool;

    /// `td_->user_manager_->get_user_stories_hidden`.
    fn get_user_stories_hidden(&self, user_id: UserId) -> bool;

    /// `td_->chat_manager_->get_channel_status`.
    fn get_channel_status(&self, channel_id: i64) -> crate::chat_manager::DialogParticipantStatus;

    /// `td_->chat_manager_->is_monoforum_channel` —
    /// [blocked: monoforum surface]; the interim answer is `false`.
    fn is_monoforum_channel(&self, channel_id: i64) -> bool {
        let _ = channel_id;
        false
    }

    /// `td_->chat_manager_->is_megagroup_channel` — the base's channel
    /// cache answers it for known channels only; the story plane reads it
    /// through the env.
    fn is_megagroup_channel(&self, channel_id: i64) -> bool;

    /// `td_->chat_manager_->can_get_channel_story_statistics`
    /// (ChatManager.cpp:3909-3921) — the story-statistics predicate
    /// rides the channel record/full read through the env.
    fn can_get_channel_story_statistics(&self, channel_id: i64) -> bool {
        let _ = channel_id;
        false
    }

    /// `UserManager::on_update_user_max_read_story_id` —
    /// [blocked: M.x user stories]; the interim no-op logs.
    fn on_update_user_max_read_story_id(&mut self, user_id: UserId, max_read_story_id: StoryId) {
        let _ = (user_id, max_read_story_id);
        tdutils::log_info!("on_update_user_max_read_story_id — [blocked: user stories]");
    }

    /// `UserManager::on_update_user_has_pinned_stories` — [blocked: user
    /// stories].
    fn on_update_user_has_pinned_stories(&mut self, user_id: UserId, has_pinned_stories: bool) {
        let _ = (user_id, has_pinned_stories);
        tdutils::log_info!("on_update_user_has_pinned_stories — [blocked: user stories]");
    }

    /// `UserManager::on_update_user_stories_hidden` — [blocked: user
    /// stories].
    fn on_update_user_stories_hidden(&mut self, user_id: UserId, stories_hidden: bool) {
        let _ = (user_id, stories_hidden);
        tdutils::log_info!("on_update_user_stories_hidden — [blocked: user stories]");
    }

    /// `ChatManager::on_update_channel_max_read_story_id` — [blocked:
    /// channel stories].
    fn on_update_channel_max_read_story_id(&mut self, channel_id: i64, max_read_story_id: StoryId) {
        let _ = (channel_id, max_read_story_id);
        tdutils::log_info!("on_update_channel_max_read_story_id — [blocked: channel stories]");
    }

    /// `ChatManager::on_update_channel_has_pinned_stories` — [blocked:
    /// channel stories].
    fn on_update_channel_has_pinned_stories(&mut self, channel_id: i64, has_pinned_stories: bool) {
        let _ = (channel_id, has_pinned_stories);
        tdutils::log_info!("on_update_channel_has_pinned_stories — [blocked: channel stories]");
    }

    /// `ChatManager::on_update_channel_stories_hidden` — [blocked: channel
    /// stories].
    fn on_update_channel_stories_hidden(&mut self, channel_id: i64, stories_hidden: bool) {
        let _ = (channel_id, stories_hidden);
        tdutils::log_info!("on_update_channel_stories_hidden — [blocked: channel stories]");
    }

    /// `UserManager::can_poll_user_active_stories` — [blocked: M.x user
    /// stories]; the interim answer is the valid-user check only.
    fn can_poll_user_active_stories(&mut self, user_id: UserId) -> bool {
        user_id.is_valid()
    }

    /// `ChatManager::can_poll_channel_active_stories` — [blocked: channel
    /// stories]; interim
    /// `false`.
    fn can_poll_channel_active_stories(&mut self, channel_id: i64) -> bool {
        let _ = channel_id;
        false
    }

    /// `MessagesManager::on_get_message(DialogId(), message, ...)` for the
    /// public-forward viewers — [blocked: MessagesManager track]; `None`
    /// models the unresolvable message and keeps the viewer invalid.
    #[allow(clippy::type_complexity)]
    fn on_get_public_forward_message(
        &mut self,
        message: Option<telegram_api::Message>,
    ) -> Option<(DialogId, crate::message_id::MessageId, i32)> {
        let _ = message;
        tdutils::log_info!(
            "on_get_message for storyViewPublicForward — [blocked: MessagesManager]"
        );
        None
    }

    /// `MessagesManager::get_dialog_message_sender` — [blocked: M.6].
    fn get_dialog_message_sender(&mut self, dialog_id: DialogId, message_id: i64) -> DialogId {
        let _ = message_id;
        dialog_id
    }

    /// `MessagesManager::get_message_object` — [blocked: MessagesManager].
    fn get_message_object(
        &mut self,
        message_full_id: Option<(DialogId, crate::message_id::MessageId)>,
        source: &str,
    ) -> Option<td_api::Message> {
        let _ = (message_full_id, source);
        tdutils::log_info!("get_message_object — [blocked: MessagesManager]");
        None
    }

    /// `get_message_sender_object` (MessageSender.cpp:60-66) — [blocked:
    /// M.6 sender surface]; the interim answer is `None` (the C++ CHECK
    /// would fire on the missing sender).
    fn get_message_sender_object(
        &mut self,
        dialog_id: DialogId,
        source: &str,
    ) -> Option<td_api::MessageSender> {
        let _ = (dialog_id, source);
        tdutils::log_info!("get_message_sender_object — [blocked: MessageSender]");
        None
    }

    /// `MessagesManager::on_update_dialog_is_blocked` — [blocked: M.6].
    fn on_update_dialog_is_blocked(
        &mut self,
        dialog_id: DialogId,
        is_blocked: bool,
        is_blocked_for_stories: bool,
    ) {
        let _ = (dialog_id, is_blocked, is_blocked_for_stories);
        tdutils::log_info!("on_update_dialog_is_blocked — [blocked: MessagesManager]");
    }

    /// `td_->dialog_manager_->get_chats_object` for the chats answers —
    /// [blocked: M.6 chats object].
    fn get_chats_object(
        &mut self,
        total_count: i32,
        dialog_ids: &[DialogId],
        source: &str,
    ) -> td_api::Chats {
        let _ = (total_count, dialog_ids, source);
        tdutils::log_info!("get_chats_object — [blocked: M.6 DialogManager]");
        td_api::Chats::default()
    }

    // --- the M.12 file plane ---

    /// `FileManager::get_input_file_id(FileType, InputFile, ...)` — the
    /// collapsed 9-argument C++ form keeps the story call's arguments.
    fn get_input_file_id(
        &mut self,
        file_type: FileType,
        file: Option<&td_api::InputFile>,
        owner_dialog_id: DialogId,
        allow_zero: bool,
        is_encrypted: bool,
    ) -> Result<FileId>;

    /// `FileManager::copy_file_id`.
    fn copy_file_id(
        &mut self,
        file_id: FileId,
        file_type: FileType,
        owner_dialog_id: DialogId,
        source: &str,
    ) -> FileId;

    /// `FileManager::check_local_location_async` — [blocked: M.12 async
    /// check]; interim no-op.
    fn check_local_location_async(&mut self, file_id: FileId) {
        let _ = file_id;
        tdutils::log_info!("check_local_location_async — [blocked: M.12 FileManager]");
    }

    /// `FileManager::cancel_upload` — [blocked: FileUploader].
    fn cancel_upload(&mut self, file_id: FileId) {
        let _ = file_id;
        tdutils::log_info!("cancel_upload — [blocked: FileUploader]");
    }

    /// `FileManager::delete_partial_remote_location` — [blocked:
    /// FileUploader].
    fn delete_partial_remote_location(&mut self, file_id: FileId) {
        let _ = file_id;
        tdutils::log_info!("delete_partial_remote_location — [blocked: FileUploader]");
    }

    /// `VideosManager::store_video` — the M.12 `FileManager::store_file`
    /// collapse.
    fn videos_store_video(&mut self, file_id: FileId, out: &mut Vec<u8>);

    /// `VideosManager::parse_video` — the M.12 `FileManager::parse_file`
    /// collapse; the parse-plane error is the string form.
    fn videos_parse_video(
        &mut self,
        parser: &mut Parser,
    ) -> std::result::Result<FileId, &'static str>;

    /// `DocumentsManager::on_get_document(..., Type::Video, Subtype::Story)`
    /// — [blocked: Documents/Videos managers]; `None` models the invalid
    /// document.
    fn on_get_story_document(
        &mut self,
        document: telegram_api::document,
        owner_dialog_id: DialogId,
    ) -> Option<FileId> {
        let _ = (document, owner_dialog_id);
        tdutils::log_info!("on_get_document(Story) — [blocked: DocumentsManager]");
        None
    }

    /// `Document::append_file_ids` for the story documents — [blocked:
    /// Documents managers]; interim empty.
    fn get_story_document_file_ids(&mut self, file_id: FileId) -> Option<Vec<FileId>> {
        let _ = file_id;
        None
    }

    /// `VideosManager::get_story_video_object` — flipped with M.28.
    fn videos_get_story_video_object(&mut self, file_id: FileId) -> Option<td_api::StoryVideo> {
        let _ = file_id;
        None
    }

    /// `VideosManager::get_video_object` (VideosManager.cpp:249-283) —
    /// the full video object of the album icon; `None` for the invalid
    /// file id.
    fn videos_get_video_object(&mut self, file_id: FileId) -> Option<td_api::Video> {
        let _ = file_id;
        None
    }

    /// `VideosManager::get_video_duration`.
    fn videos_get_video_duration(&mut self, file_id: FileId) -> i32;

    /// `VideosManager::create_video` for the story geometry — [blocked:
    /// VideosManager record]; interim no-op.
    #[allow(clippy::too_many_arguments)]
    fn videos_create_story_video(
        &mut self,
        file_id: FileId,
        duration_seconds: i32,
        duration: f64,
        dimensions: crate::photo::Dimensions,
        is_animation: bool,
        cover_frame_timestamp: f64,
        has_stickers: bool,
        sticker_file_ids: Vec<FileId>,
    ) {
        let _ = (
            file_id,
            duration_seconds,
            duration,
            dimensions,
            is_animation,
            cover_frame_timestamp,
            has_stickers,
            sticker_file_ids,
        );
        tdutils::log_info!("create_video(Story) — [blocked: VideosManager]");
    }

    /// `StickersManager::get_attached_sticker_file_ids` — [blocked:
    /// StickersManager]; interim empty.
    fn get_attached_sticker_file_ids(&mut self, sticker_file_ids: &[i32]) -> Vec<FileId> {
        let _ = sticker_file_ids;
        Vec::new()
    }

    /// `merge_photos` (Photo.cpp) — [blocked: photo merge]; interim: the
    /// merge never changes the old photo.
    fn merge_photos(
        &mut self,
        old_photo: &mut Photo,
        new_photo: &mut Photo,
        dialog_id: DialogId,
        is_stories: bool,
    ) -> (bool, bool) {
        let _ = (old_photo, new_photo, dialog_id, is_stories);
        tdutils::log_info!("merge_photos — [blocked: photo track]");
        (false, false)
    }

    /// `GroupCallManager::get_group_call_id` — [blocked: GroupCallManager];
    /// the interim id is the raw call id.
    fn get_group_call_id(
        &mut self,
        call_id: crate::story_content::InputGroupCallId,
        dialog_id: DialogId,
    ) -> i32 {
        let _ = dialog_id;
        call_id.get_group_call_id() as i32
    }

    // --- the StoryDb seam ---

    /// `G()->td_db()->get_story_db_sync()` — `None` models the disabled
    /// message database.
    fn story_db(&mut self) -> Option<&mut StoryDb>;

    // --- the net query seams ([blocked: net]) ---

    /// `td_->dialog_manager_->get_input_peer` (DialogManager.cpp:1377-1395)
    /// — the peer resolution of the story query send legs.
    fn get_input_peer(
        &mut self,
        dialog_id: DialogId,
        access_rights: crate::user_manager::AccessRights,
    ) -> Option<tdmtproto::telegram_api::InputPeer>;

    /// `td_->user_manager_->get_input_users_force`
    /// (UserManager.cpp:5254-5265) — the story privacy rules' user legs.
    fn get_input_users_force(
        &mut self,
        user_ids: &[UserId],
    ) -> Vec<tdmtproto::telegram_api::InputUser>;

    /// `td_->dialog_manager_->on_get_dialog_error` (DialogManager.cpp
    /// `on_get_dialog_error`) — the error heads of the story handlers.
    fn on_get_dialog_error(&mut self, dialog_id: DialogId, status: &Status, source: &str);

    /// `GetStoryViewsListQuery::send` (StoryManager.cpp:312-327).
    #[allow(clippy::too_many_arguments)]
    fn send_get_story_views_list_query(
        &mut self,
        dialog_id: DialogId,
        story_id: StoryId,
        query: &str,
        only_contacts: bool,
        prefer_forwards: bool,
        prefer_with_reaction: bool,
        offset: &str,
        limit: i32,
    ) {
        let _ = (
            dialog_id,
            story_id,
            only_contacts,
            prefer_forwards,
            prefer_with_reaction,
        );
        tdutils::log_info!("Send stories.getStoryViewsList — [net-inert]");
        let _ = (query, offset, limit);
    }

    /// `GetStoriesViewsQuery::send` (StoryManager.cpp) — the interaction
    /// count poll.
    fn send_get_stories_views_query(&mut self, dialog_id: DialogId, story_ids: &[StoryId]) {
        let _ = dialog_id;
        tdutils::log_info!(
            "Send stories.getStoriesViews({} ids) — [net-inert]",
            story_ids.len()
        );
    }

    /// `GetStoryReactionsListQuery::send` (StoryManager.cpp:356-375).
    fn send_get_story_reactions_list_query(
        &mut self,
        story_full_id: StoryFullId,
        reaction_type: &ReactionType,
        prefer_forwards: bool,
        offset: &str,
        limit: i32,
    ) {
        let _ = (story_full_id, prefer_forwards);
        tdutils::log_info!("Send stories.getStoryReactionsList — [net-inert]");
        let _ = (reaction_type, offset, limit);
    }

    /// `GetStoriesByIDQuery::send` (StoryManager.cpp:403-412).
    fn send_get_stories_by_id_query(&mut self, dialog_id: DialogId, story_ids: &[StoryId]) {
        let _ = dialog_id;
        tdutils::log_info!(
            "Send stories.getStoriesByID({} ids) — [net-inert]",
            story_ids.len()
        );
    }

    /// `GetPinnedStoriesQuery::send` (StoryManager.cpp:441-449).
    fn send_get_pinned_stories_query(
        &mut self,
        dialog_id: DialogId,
        offset_story_id: StoryId,
        limit: i32,
    ) {
        let _ = dialog_id;
        tdutils::log_info!("Send stories.getPinnedStories — [net-inert]");
        let _ = (offset_story_id, limit);
    }

    /// `GetStoriesArchiveQuery::send` (StoryManager.cpp:477-485).
    fn send_get_stories_archive_query(
        &mut self,
        dialog_id: DialogId,
        offset_story_id: StoryId,
        limit: i32,
    ) {
        let _ = dialog_id;
        tdutils::log_info!("Send stories.getStoriesArchive — [net-inert]");
        let _ = (offset_story_id, limit);
    }

    /// `SearchStoriesQuery::send` for the three search arms
    /// (StoryManager.cpp:3212-3237) — the arm carries the wire's
    /// hashtag/area payload (the C++ `send` overloads); `dialog_id` is
    /// the hashtag arm's peer filter (default for the area arms).
    fn send_search_stories_query(
        &mut self,
        dialog_id: DialogId,
        arm: SearchStoriesArm<'_>,
        offset: &str,
        limit: i32,
    ) {
        let _ = (dialog_id, offset, limit);
        tdutils::log_info!("Send stories.searchPosts — [net-inert]");
        let _ = arm;
    }

    /// `GetStoryAlbumsQuery::send` (StoryManager.cpp:3855).
    fn send_get_story_albums_query(&mut self, dialog_id: DialogId) {
        let _ = dialog_id;
        tdutils::log_info!("Send stories.getAlbums — [net-inert]");
    }

    /// `GetAlbumStoriesQuery::send` (StoryManager.cpp:3882).
    fn send_get_album_stories_query(
        &mut self,
        dialog_id: DialogId,
        story_album_id: StoryAlbumId,
        offset: i32,
        limit: i32,
    ) {
        let _ = dialog_id;
        tdutils::log_info!("Send stories.getAlbumStories — [net-inert]");
        let _ = (story_album_id, offset, limit);
    }

    /// `CreateStoryAlbumQuery::send` (StoryManager.cpp:3923).
    fn send_create_story_album_query(
        &mut self,
        dialog_id: DialogId,
        title: &str,
        story_ids: &[StoryId],
    ) {
        let _ = dialog_id;
        tdutils::log_info!("Send stories.createAlbum — [net-inert]");
        let _ = (title, story_ids);
    }

    /// `ReorderStoryAlbumsQuery::send` (StoryManager.cpp:3932).
    fn send_reorder_story_albums_query(
        &mut self,
        dialog_id: DialogId,
        story_album_ids: &[StoryAlbumId],
    ) {
        let _ = dialog_id;
        tdutils::log_info!(
            "Send stories.reorderAlbums({} ids) — [net-inert]",
            story_album_ids.len()
        );
    }

    /// `DeleteStoryAlbumQuery::send` (StoryManager.cpp:3938).
    fn send_delete_story_album_query(&mut self, dialog_id: DialogId, story_album_id: StoryAlbumId) {
        let _ = dialog_id;
        tdutils::log_info!("Send stories.deleteAlbum — [net-inert]");
        let _ = story_album_id;
    }

    /// `UpdateStoryAlbumQuery::send` (StoryManager.cpp:3948-3981) — the
    /// title/add/remove/reorder arms share the query.
    #[allow(clippy::too_many_arguments)]
    fn send_update_story_album_query(
        &mut self,
        dialog_id: DialogId,
        story_album_id: StoryAlbumId,
        title: &str,
        removed_story_ids: &[StoryId],
        added_story_ids: &[StoryId],
        reorder_story_ids: &[StoryId],
    ) {
        let _ = dialog_id;
        tdutils::log_info!("Send stories.updateAlbum — [net-inert]");
        let _ = (
            story_album_id,
            title,
            removed_story_ids,
            added_story_ids,
            reorder_story_ids,
        );
    }

    /// `CanSendStoryQuery::send` (StoryManager.cpp:5897).
    fn send_can_send_story_query(&mut self, dialog_id: DialogId) {
        let _ = dialog_id;
        tdutils::log_info!("Send channels.getSendStoriesPaidReactionInfo — [net-inert]");
    }

    /// `StartLiveStoryQuery::send` (StoryManager.cpp:6226).
    #[allow(clippy::too_many_arguments)]
    fn send_start_live_story_query(
        &mut self,
        dialog_id: DialogId,
        privacy_rules: &UserPrivacySettingRules,
        is_pinned: bool,
        protect_content: bool,
        is_rtmp_stream: bool,
        enable_messages: bool,
        paid_message_star_count: i64,
    ) {
        let _ = dialog_id;
        tdutils::log_info!("Send stories.startLiveStory — [net-inert]");
        let _ = (
            privacy_rules,
            is_pinned,
            protect_content,
            is_rtmp_stream,
            enable_messages,
            paid_message_star_count,
        );
    }

    /// `GetChatsToSendStoriesQuery::send` (StoryManager.cpp:5820).
    fn send_get_chats_to_send_stories_query(&mut self) {
        tdutils::log_info!("Send channels.getChannelsToSendStories — [net-inert]");
    }

    /// `StoryManager::EditBusinessStoryQuery::send`
    /// (StoryManager.cpp:1676-1695) — `stories.editStory` with the
    /// full media/areas/caption/privacy flags; the send story num keys
    /// the parked `BeingEditedBusinessStory` (the kind rides it back at
    /// the answer).
    #[allow(clippy::too_many_arguments)]
    fn send_edit_business_story_query(
        &mut self,
        dialog_id: DialogId,
        story_id: StoryId,
        input_media: telegram_api::InputMedia,
        areas: &[MediaArea],
        caption: &FormattedText,
        privacy_rules: &UserPrivacySettingRules,
        edit_story_num: u32,
    ) {
        let _ = (
            dialog_id,
            story_id,
            input_media,
            areas,
            caption,
            privacy_rules,
            edit_story_num,
        );
        tdutils::log_info!("Send stories.editStory (business) — [net-inert]");
    }

    /// `td_->chat_manager_->get_channel_ids(chats, source)` — the
    /// `on_get_dialogs_to_send_stories` channel extraction (the chats
    /// land through the real intake).
    fn get_channel_ids(&mut self, chats: Vec<telegram_api::Chat>, source: &str) -> Vec<i64> {
        let _ = (chats, source);
        Vec::new()
    }

    /// `get_story_content_input_media(td, content, input_file)`
    /// (StoryContent.cpp:351-372) narrowed to the uploaded-input-file-less
    /// form the collapsed edit send uses — the photo/video remote arms;
    /// `None` is the fresh-upload band (the FileUploader plane).
    fn get_story_content_input_media(
        &mut self,
        content: &StoryContent,
    ) -> Option<telegram_api::InputMedia> {
        let _ = content;
        tdutils::log_info!("get_story_content_input_media — [net-inert]");
        None
    }

    /// `EditStoryCoverQuery::send` (StoryManager.cpp:540-608) — [blocked:
    /// FileUploader + StoryContent → input media] (the cover re-upload).
    fn send_edit_story_cover_query(
        &mut self,
        story_full_id: StoryFullId,
        main_frame_timestamp: f64,
    ) {
        let _ = story_full_id;
        tdutils::log_info!("Send stories.editStory(cover) — [blocked: net]");
        let _ = main_frame_timestamp;
    }

    /// `ReactionManager::is_active_reaction` — [blocked: ReactionManager];
    /// the interim answer trusts the non-empty check.
    fn is_active_reaction(&mut self, reaction_type: &ReactionType) -> bool {
        !reaction_type.is_empty()
    }

    /// `ReactionManager::add_recent_reaction` — [blocked: ReactionManager].
    fn add_recent_reaction(&mut self, reaction_type: &ReactionType) {
        let _ = reaction_type;
        tdutils::log_info!("add_recent_reaction — [blocked: ReactionManager]");
    }
}

/// `StoryManager::PendingStory` (StoryManager.h:125-146) — the send/edit
/// machine's parked unit; the upload halves are [blocked: FileUploader].
#[derive(Clone, Debug)]
pub struct PendingStory {
    pub dialog_id: DialogId,
    pub story_id: StoryId,
    pub forward_from_story_full_id: StoryFullId,
    pub send_story_num: u32,
    pub random_id: i64,
    pub was_reuploaded: bool,
    pub story: Story,
}

/// The `process_updates_users_and_chats` legs the §3.5 Updates answers
/// run (`UpdatesManager::process_updates_users_and_chats`,
/// UpdatesManager.cpp:6617-6636) — the users/chats of the umbrella land
/// through the env intakes (the full sink surface is the updates plane).
fn on_updates_users_and_chats(env: &mut impl StoryManagerEnv, updates: &mut telegram_api::Updates) {
    let (users, chats) = match updates {
        telegram_api::Updates::updatesCombined(updates) => (
            std::mem::take(&mut updates.users),
            std::mem::take(&mut updates.chats),
        ),
        telegram_api::Updates::updates(updates) => (
            std::mem::take(&mut updates.users),
            std::mem::take(&mut updates.chats),
        ),
        _ => return,
    };
    env.on_get_users(users, "process_updates_users_and_chats");
    env.on_get_chats(chats, "process_updates_users_and_chats");
}

/// The three `SearchStoriesQuery::send` arms (StoryManager.cpp:724-785)
/// — one wire query (`stories.searchPosts`), three payload legs: the
/// `searchPublicStoriesByTag` hashtag, the
/// `searchPublicStoriesByLocation` geo-point address and the
/// `searchPublicStoriesByVenue` provider/id pair.
pub enum SearchStoriesArm<'a> {
    /// The hashtag leg — the `#`/`$`-prefixed query string.
    Hashtag { query: &'a str },
    /// The location leg — the address the wire `mediaAreaGeoPoint`
    /// carries.
    Location {
        address: &'a td_api::LocationAddress,
    },
    /// The venue leg.
    Venue {
        venue_provider: &'a str,
        venue_id: &'a str,
    },
}

/// The [`StoryManager::increment_story_views`] free-standing body — the
/// manager method keeps the has-query bookkeeping; the query send rides the
/// env seam.
/// `StoryManager::increment_story_views` (StoryManager.cpp:3555-3573) —
/// the drain + the `IncrementStoryViewsQuery` send (the promise lambda
/// re-enters `on_increment_story_views` on both bands).
fn increment_story_views_impl(
    manager: &mut StoryManager,
    env: &mut impl StoryManagerEnv,
    owner_dialog_id: DialogId,
) {
    let mut viewed_story_ids = Vec::new();
    const MAX_VIEWED_STORIES: usize = 200; // server-side limit
    {
        let Some(story_views) = manager.pending_story_views.get_mut(&owner_dialog_id) else {
            return;
        };
        while !story_views.story_ids.is_empty() && viewed_story_ids.len() < MAX_VIEWED_STORIES {
            let story_id = *story_views.story_ids.iter().next().expect("non-empty");
            story_views.story_ids.remove(&story_id);
            viewed_story_ids.push(StoryId::new(story_id));
        }
        assert!(!viewed_story_ids.is_empty());
        assert!(!story_views.has_query);
        story_views.has_query = true;
    }
    // `IncrementStoryViewsQuery::send` (StoryManager.cpp:198-207) — the
    // Read peer; the null band is the C++ on_error head, then the
    // lambda's `on_increment_story_views` (the ids are drained, so the
    // re-arm collapses like the C++).
    match env.get_input_peer(owner_dialog_id, crate::user_manager::AccessRights::Read) {
        Some(input_peer) => manager.send_story_net_query(
            StoryNetKind::IncrementStoryViews {
                dialog_id: owner_dialog_id,
            },
            telegram_api::Function::stories_incrementStoryViews(
                telegram_api::stories_incrementStoryViews {
                    peer: Some(Box::new(input_peer)),
                    id: viewed_story_ids
                        .iter()
                        .map(|story_id| story_id.get())
                        .collect(),
                },
            ),
            "stories.incrementStoryViews",
        ),
        None => {
            let status = Status::error(400, "Can't access the chat");
            env.on_get_dialog_error(owner_dialog_id, &status, "IncrementStoryViewsQuery");
            manager.on_increment_story_views(env, owner_dialog_id);
        }
    }
}

/// The port of `td::StoryManager`.
#[derive(Default)]
pub struct StoryManager {
    /// The start_up-once guard (the C++ constructor + start_up collapse).
    is_inited: bool,

    /// `stories_`.
    stories: HashMap<StoryFullId, Story>,
    /// `stories_by_global_id_`.
    stories_by_global_id: HashMap<i64, StoryFullId>,
    /// `inaccessible_story_full_ids_` — the id → the last reload time.
    inaccessible_story_full_ids: HashMap<StoryFullId, f64>,
    /// `deleted_story_full_ids_`.
    deleted_story_full_ids: std::collections::HashSet<StoryFullId>,
    /// `failed_to_load_story_full_ids_`.
    failed_to_load_story_full_ids: std::collections::HashSet<StoryFullId>,
    /// `story_full_id_to_file_source_id_` — [blocked: FileSource
    /// unification]; the interim map pins the id-existence only.
    story_full_id_to_file_source_id: HashMap<StoryFullId, ()>,
    /// `story_album_full_id_to_file_source_id_` — [blocked: FileSource
    /// unification]; written by [`Self::register_story_album`], read when
    /// the file-source plane lands.
    #[allow(dead_code)] // kept for the C++ field parity; reads land with FileSource
    story_album_full_id_to_file_source_id: HashMap<(DialogId, StoryAlbumId), ()>,
    /// `story_album_file_ids_`.
    story_album_file_ids: HashMap<(DialogId, StoryAlbumId), Vec<FileId>>,

    /// `active_stories_`.
    active_stories: HashMap<DialogId, ActiveStories>,
    /// `updated_active_stories_`.
    updated_active_stories: std::collections::HashSet<DialogId>,
    /// `max_read_story_ids_`.
    max_read_story_ids: HashMap<DialogId, StoryId>,
    /// `failed_to_load_active_stories_`.
    failed_to_load_active_stories: std::collections::HashSet<DialogId>,
    /// `load_expiring_stories_log_event_ids_` — the binlog ids are the
    /// [blocked: binlog] zero stub.
    load_expiring_stories_dialog_ids: std::collections::HashSet<DialogId>,

    /// `being_edited_stories_`.
    being_edited_stories: HashMap<StoryFullId, BeingEditedStory>,
    /// `edit_generations_`.
    edit_generations: HashMap<StoryFullId, i64>,
    /// `pending_story_views_`.
    pending_story_views: HashMap<DialogId, PendingStoryViews>,
    /// `opened_stories_with_view_count_`.
    opened_stories_with_view_count: HashMap<StoryFullId, u32>,
    /// `opened_stories_`.
    opened_stories: HashMap<StoryFullId, u32>,
    /// `reload_story_queries_`.
    reload_story_queries: HashMap<StoryFullId, Vec<Promise<()>>>,

    /// `yet_unsent_stories_`.
    yet_unsent_stories: HashMap<DialogId, BTreeSet<u32>>,
    /// `yet_unsent_story_ids_`.
    yet_unsent_story_ids: HashMap<DialogId, Vec<StoryId>>,
    /// `being_sent_stories_`.
    being_sent_stories: HashMap<i64, StoryFullId>,
    /// `being_sent_story_random_ids_`.
    being_sent_story_random_ids: HashMap<StoryFullId, i64>,
    /// `update_story_ids_`.
    update_story_ids: HashMap<StoryFullId, StoryId>,
    /// `current_yet_unsent_story_ids_`.
    current_yet_unsent_story_ids: HashMap<DialogId, i32>,
    /// `being_set_story_reactions_`.
    being_set_story_reactions: HashMap<StoryFullId, u32>,

    /// `story_lists_[2]` — main first, archive second (StoryManager.cpp:2882).
    story_lists: [StoryList; 2],

    /// `stealth_mode_`.
    stealth_mode: StoryStealthMode,

    /// `send_story_count_`.
    send_story_count: u32,
    /// `max_story_global_id_`.
    max_story_global_id: i64,
    /// `has_active_synchronize_archive_all_stories_query_`.
    has_active_synchronize_archive_all_stories_query: bool,

    /// `channels_to_send_stories_inited_`.
    channels_to_send_stories_inited: bool,
    /// `channels_to_send_stories_`.
    channels_to_send_stories: Vec<i64>,

    /// The parked `getDialogsToSendStories` promises
    /// (`get_dialogs_to_send_stories_queries_`).
    pending_dialogs_to_send_stories: Vec<Promise<td_api::Chats>>,
    /// The parked `loadActiveStories` promises
    /// (`load_list_from_database_queries_`/`load_list_from_server_queries_`).
    pending_load_list: HashMap<StoryListId, Vec<Promise<()>>>,
    /// The parked pinned-stories promises.
    pending_pinned_stories: HashMap<DialogId, Vec<Promise<td_api::Stories>>>,
    /// The parked archive-stories promises.
    pending_archive_stories: HashMap<DialogId, Vec<Promise<td_api::Stories>>>,
    /// The parked album-stories promises.
    pending_album_stories: HashMap<(DialogId, StoryAlbumId), Vec<Promise<td_api::Stories>>>,
    /// The parked chat-active-stories promises.
    pending_expiring_stories: HashMap<DialogId, Vec<Promise<td_api::ChatActiveStories>>>,
    /// The parked story-albums promises.
    pending_story_albums: HashMap<DialogId, Vec<Promise<td_api::StoryAlbums>>>,
    /// The parked `getStoryInteractions` promises.
    pending_story_interactions: Vec<Promise<td_api::StoryInteractions>>,
    /// The parked `getChatStoryInteractions` promises.
    pending_dialog_story_interactions: Vec<Promise<td_api::StoryInteractions>>,
    /// The parked `setStoryReaction` promises.
    pending_story_reactions: HashMap<StoryFullId, Vec<Promise<()>>>,
    /// The parked `activateStoryStealthMode` promises.
    pending_stealth_mode: Vec<Promise<()>>,
    /// The parked `getStory` promises riding the reload chain.
    pending_get_story: HashMap<StoryFullId, Vec<Promise<Option<td_api::Story>>>>,
    /// The parked `searchPublicStories*` promises.
    pending_search_stories: Vec<Promise<td_api::FoundStories>>,
    /// The parked `canPostStory` promises.
    pending_can_post_story: Vec<Promise<td_api::CanPostStoryResult>>,
    /// The parked `startLiveStory` promises.
    pending_start_live_story: Vec<Promise<td_api::StartLiveStoryResult>>,
    /// The parked `reportStory` promises.
    pending_report_story: Vec<Promise<td_api::ReportStoryResult>>,
    /// The parked unit-answer query promises.
    pending_unit_queries: Vec<Promise<()>>,
    /// The net dispatch surface — the `td_` back-pointer the C++ manager
    /// uses for its query sends, narrowed to the net plane (`None` in
    /// the net-inert tests; set at the dispatcher install).
    pub(crate) net: Option<crate::td::StoryNetHandle>,
    /// The in-flight `GetAllStoriesQuery` snapshots — the C++ promise
    /// lambda's `is_next`/old-state captures (StoryManager.cpp:2730-2743).
    in_flight_load_list: HashMap<StoryListId, (bool, String)>,
    /// The parked `toggleChatStoriesHidden` promises (the C++ handler's
    /// `promise_`, StoryManager.cpp:104).
    pending_toggle_stories_hidden: Vec<Promise<()>>,
    /// The parked `setStoryPrivacySettings` promises
    /// (EditStoryPrivacyQuery's `promise_`, StoryManager.cpp:611).
    pending_edit_story_privacy: Vec<Promise<()>>,
    /// The parked `toggleStoryIsPostedToChatPage` promises
    /// (ToggleStoryPinnedQuery's `promise_`, StoryManager.cpp:655).
    pending_toggle_story_pinned: Vec<Promise<()>>,
    /// The parked `setChatPinnedStories` promises
    /// (TogglePinnedStoriesToTopQuery's `promise_`, StoryManager.cpp:826).
    pending_set_pinned_stories: Vec<Promise<()>>,
    /// The parked `deleteStory` promises (DeleteStoriesQuery's `promise_`,
    /// StoryManager.cpp:691) — the query-carried promise restored per §5
    /// (the interim settled inline at the send).
    pending_delete_stories: Vec<Promise<()>>,
    /// The in-flight `SendStoryQuery` tails — the C++ handler's
    /// `pending_story_` member (the `delete_pending_story` lambda
    /// capture, StoryManager.cpp:1336).
    pending_send_story_tail: Vec<PendingStory>,
    /// The parked `postStory`-repost promises
    /// (RepostBusinessStoryQuery's `promise_`, StoryManager.cpp:1441) —
    /// the query-carried promise restored per §5 (the interim answered
    /// `None` at the send).
    pending_repost_stories: Vec<Promise<Option<td_api::Story>>>,
    /// The parked Reorder/DeleteStoryAlbumQuery promises — the Bool
    /// answers of the §3.5 slice (kept off [`Self::pending_unit_queries`]
    /// whose still-inert families must not settle on these answers).
    pending_story_album_unit_queries: Vec<Promise<()>>,
    /// The GetStoriesViewsQuery context queue — the handler's
    /// `dialog_id_`/`story_ids_` members (the answer runs
    /// `on_get_story_views` with them).
    pending_get_stories_views: Vec<(DialogId, Vec<StoryId>)>,
    /// The parked CreateStoryAlbumQuery promises (the handler member).
    pending_create_story_albums: Vec<Promise<td_api::StoryAlbum>>,
    /// The CreateStoryAlbumQuery context queue — the handler's
    /// `owner_dialog_id_`/`story_ids_` members (the answer's
    /// `update_story_albums` refire reads them).
    pending_create_story_album_contexts: Vec<(DialogId, Vec<StoryId>)>,
    /// The parked UpdateStoryAlbumQuery promises (the handler member).
    pending_update_story_albums: Vec<Promise<td_api::StoryAlbum>>,
    /// The UpdateStoryAlbumQuery context queue — the handler's
    /// `owner_dialog_id_`/`story_album_id_`/`deleted_story_ids_`/
    /// `added_story_ids_` members (the answer's id check and the error
    /// tail's `update_story_albums` rollback read them).
    pending_update_story_album_contexts: Vec<(DialogId, StoryAlbumId, Vec<StoryId>, Vec<StoryId>)>,
    /// `being_edited_business_stories_` — the business edit's parked
    /// rows keyed by the send story num.
    being_edited_business_stories: HashMap<u32, BeingEditedBusinessStory>,
    /// `load_expired_database_stories_next_limit_`.
    load_expired_database_stories_next_limit: i32,
}

impl StoryManager {
    /// `StoryManager::StoryManager` (StoryManager.cpp:1999-2030).
    pub fn new() -> StoryManager {
        StoryManager::default()
    }

    /// The shared dispatch tail — `send_query(G()->net_query_creator()…)`
    /// narrowed to the handle; the None arm is the net-inert log (the
    /// `ChatManager::send_chat_net_query` shape).
    pub(crate) fn send_story_net_query(
        &self,
        kind: StoryNetKind,
        function: tdmtproto::telegram_api::Function,
        what: &str,
    ) {
        match &self.net {
            Some(net) => net.dispatch(kind, function),
            None => tdutils::log_info!("Send {what} — [net-inert]"),
        }
    }

    /// The FIFO family-queue pop shared by the `take_*_promise` heads
    /// (the §5 answer→promise pairing).
    fn pop_head<T>(queue: &mut Vec<T>) -> Option<T> {
        if queue.is_empty() {
            None
        } else {
            Some(queue.remove(0))
        }
    }

    /// `ToggleStoriesHiddenQuery::on_result`'s promise pop
    /// (StoryManager.cpp:104).
    pub(crate) fn take_toggle_stories_hidden_promise(&mut self) -> Option<Promise<()>> {
        Self::pop_head(&mut self.pending_toggle_stories_hidden)
    }

    /// `EditStoryPrivacyQuery::on_result`'s promise pop
    /// (StoryManager.cpp:611).
    pub(crate) fn take_edit_story_privacy_promise(&mut self) -> Option<Promise<()>> {
        Self::pop_head(&mut self.pending_edit_story_privacy)
    }

    /// `ToggleStoryPinnedQuery::on_result`'s promise pop
    /// (StoryManager.cpp:655).
    pub(crate) fn take_toggle_story_pinned_promise(&mut self) -> Option<Promise<()>> {
        Self::pop_head(&mut self.pending_toggle_story_pinned)
    }

    /// `TogglePinnedStoriesToTopQuery::on_result`'s promise pop
    /// (StoryManager.cpp:826).
    pub(crate) fn take_set_pinned_stories_promise(&mut self) -> Option<Promise<()>> {
        Self::pop_head(&mut self.pending_set_pinned_stories)
    }

    /// `DeleteStoriesQuery::on_result`'s promise pop
    /// (StoryManager.cpp:691).
    pub(crate) fn take_delete_stories_promise(&mut self) -> Option<Promise<()>> {
        Self::pop_head(&mut self.pending_delete_stories)
    }

    /// `RepostBusinessStoryQuery::on_result`'s promise pop
    /// (StoryManager.cpp:1441).
    pub(crate) fn take_repost_business_story_promise(
        &mut self,
    ) -> Option<Promise<Option<td_api::Story>>> {
        Self::pop_head(&mut self.pending_repost_stories)
    }

    /// `ActivateStealthModeQuery`'s promise pop (StoryManager.cpp:1227).
    pub(crate) fn take_stealth_mode_promise(&mut self) -> Option<Promise<()>> {
        Self::pop_head(&mut self.pending_stealth_mode)
    }

    /// `delete_pending_story`'s stale-edit guard (StoryManager.cpp
    /// :1615-1621) — `edit_generations_[id] != random_id` drops the
    /// outdated edits.
    pub(crate) fn is_outdated_edit(&self, story_full_id: StoryFullId, generation: i64) -> bool {
        self.edit_generations.get(&story_full_id) != Some(&generation)
    }

    /// `GetAllStoriesQuery`'s in-flight snapshot pop — the promise
    /// lambda's `is_next`/old-state captures (StoryManager.cpp:2745-2748).
    pub(crate) fn take_in_flight_load_list(
        &mut self,
        story_list_id: StoryListId,
    ) -> Option<(bool, String)> {
        self.in_flight_load_list.remove(&story_list_id)
    }

    /// `ReportStoryQuery::on_result`'s promise pop
    /// (StoryManager.cpp:892).
    pub(crate) fn take_report_story_promise(
        &mut self,
    ) -> Option<Promise<td_api::ReportStoryResult>> {
        Self::pop_head(&mut self.pending_report_story)
    }

    /// `SendStoryQuery`'s `pending_story_` member pop
    /// (StoryManager.cpp:1336).
    pub(crate) fn take_send_story_tail(&mut self) -> Option<PendingStory> {
        Self::pop_head(&mut self.pending_send_story_tail)
    }

    /// The `GetPeerStoriesQuery` error tail's parked-promise fail — the
    /// query-promise lambda's error arm (StoryManager.cpp:3090-3094).
    pub(crate) fn fail_expiring_stories(&mut self, dialog_id: DialogId, status: Status) {
        if let Some(promises) = self.pending_expiring_stories.remove(&dialog_id) {
            for mut promise in promises {
                promise.set_error(status.clone());
            }
        }
    }

    /// `get_story_stealth_mode_key` (StoryManager.cpp:5398-5400).
    fn get_story_stealth_mode_key() -> &'static str {
        "stealth_mode"
    }

    fn is_active(env: &impl StoryManagerEnv) -> bool {
        env.is_authorized() && !env.is_bot()
    }

    /// `is_my_story` (StoryManager.cpp:2234-2236).
    fn is_my_story(&self, env: &impl StoryManagerEnv, owner_dialog_id: DialogId) -> bool {
        owner_dialog_id == env.get_my_dialog_id()
    }

    /// `check_story_id` (StoryManager.cpp:3240-3248).
    fn check_story_id(&self, story_id: StoryId, only_server: bool) -> Status {
        if !story_id.is_valid() {
            return Status::error(400, "Invalid story identifier specified");
        }
        if only_server && !story_id.is_server() {
            return Status::error(400, "Can't use the story");
        }
        Status::ok()
    }

    /// `check_story_ids` (StoryManager.cpp:3250-3255).
    fn check_story_ids(&self, story_ids: &[StoryId], only_server: bool) -> Status {
        for story_id in story_ids {
            let status = self.check_story_id(*story_id, only_server);
            if status.is_error() {
                return status;
            }
        }
        Status::ok()
    }

    /// `check_story_album_id` (StoryManager.cpp:3257-3262).
    fn check_story_album_id(&self, story_album_id: StoryAlbumId) -> Status {
        if !story_album_id.is_valid() {
            return Status::error(400, "Invalid story album identifier specified");
        }
        Status::ok()
    }

    /// `can_access_expired_story` (StoryManager.cpp:2238-2243).
    fn can_access_expired_story(
        &self,
        env: &impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story: &Story,
    ) -> bool {
        // non-pinned non-editable stories can't be accessed after they expire
        story.is_pinned || self.can_edit_stories(env, owner_dialog_id)
    }

    /// `can_get_story_view_count` (StoryManager.cpp:2245-2257).
    fn can_get_story_view_count(
        &self,
        env: &impl StoryManagerEnv,
        owner_dialog_id: DialogId,
    ) -> bool {
        // result must be stable over time
        match owner_dialog_id.get_type() {
            DialogType::User => self.is_my_story(env, owner_dialog_id),
            _ => true,
        }
    }

    /// `can_have_stories` (StoryManager.cpp:2259-2271).
    fn can_have_stories(&self, env: &impl StoryManagerEnv, owner_dialog_id: DialogId) -> bool {
        match owner_dialog_id.get_type() {
            DialogType::User => !env.is_user_bot(owner_dialog_id.get_user_id()),
            DialogType::Channel => {
                !env.is_monoforum_channel(owner_dialog_id.get_channel_id().get())
            }
            _ => false,
        }
    }

    /// `can_post_stories` (StoryManager.cpp:2273-2286).
    fn can_post_stories(&self, env: &impl StoryManagerEnv, owner_dialog_id: DialogId) -> bool {
        match owner_dialog_id.get_type() {
            DialogType::User => self.is_my_story(env, owner_dialog_id) != env.is_bot(),
            DialogType::Channel => {
                !env.is_bot()
                    && env
                        .get_channel_status(owner_dialog_id.get_channel_id().get())
                        .can_post_stories()
            }
            _ => false,
        }
    }

    /// `can_edit_stories` (StoryManager.cpp:2288-2301).
    fn can_edit_stories(&self, env: &impl StoryManagerEnv, owner_dialog_id: DialogId) -> bool {
        match owner_dialog_id.get_type() {
            DialogType::User => self.is_my_story(env, owner_dialog_id) != env.is_bot(),
            DialogType::Channel => {
                !env.is_bot()
                    && env
                        .get_channel_status(owner_dialog_id.get_channel_id().get())
                        .can_edit_stories()
            }
            _ => false,
        }
    }

    /// `can_delete_stories` (StoryManager.cpp:2303-2316).
    fn can_delete_stories(&self, env: &impl StoryManagerEnv, owner_dialog_id: DialogId) -> bool {
        match owner_dialog_id.get_type() {
            DialogType::User => self.is_my_story(env, owner_dialog_id) != env.is_bot(),
            DialogType::Channel => {
                !env.is_bot()
                    && env
                        .get_channel_status(owner_dialog_id.get_channel_id().get())
                        .can_delete_stories()
            }
            _ => false,
        }
    }

    /// `can_edit_story` (StoryManager.cpp:2318-2328).
    fn can_edit_story(
        &self,
        env: &impl StoryManagerEnv,
        story_full_id: StoryFullId,
        story: Option<&Story>,
    ) -> bool {
        if !story_full_id.get_story_id().is_server() {
            return false;
        }
        if let Some(story) = story
            && let Some(content) = &story.content
            && !can_edit_story_content(content.get_type())
        {
            return false;
        }
        let owner_dialog_id = story_full_id.get_dialog_id();
        self.can_edit_stories(env, owner_dialog_id)
            || (story.is_some_and(|story| story.is_outgoing)
                && self.can_post_stories(env, owner_dialog_id))
    }

    /// `can_set_story_privacy_settings` (StoryManager.cpp:2330-2337).
    fn can_set_story_privacy_settings(
        &self,
        env: &impl StoryManagerEnv,
        story_full_id: StoryFullId,
        story: &Story,
    ) -> bool {
        if !story_full_id.get_story_id().is_server() {
            return false;
        }
        let owner_dialog_id = story_full_id.get_dialog_id();
        self.can_edit_stories(env, owner_dialog_id)
            || (story.is_outgoing && self.can_post_stories(env, owner_dialog_id))
    }

    /// `can_toggle_story_is_pinned` (StoryManager.cpp:2339-2344).
    fn can_toggle_story_is_pinned(
        &self,
        env: &impl StoryManagerEnv,
        story_full_id: StoryFullId,
    ) -> bool {
        if !story_full_id.get_story_id().is_server() {
            return false;
        }
        self.can_edit_stories(env, story_full_id.get_dialog_id())
    }

    /// `can_add_story_to_album` (StoryManager.cpp:2346-2352). The current
    /// intake reads [`Self::can_add_story_to_album_ref`]; this borrow-less
    /// twin answers when the callers re-borrow through the env.
    #[allow(dead_code)]
    fn can_add_story_to_album(
        &self,
        env: &impl StoryManagerEnv,
        story_full_id: StoryFullId,
        story: &Story,
    ) -> bool {
        if !story_full_id.get_story_id().is_server() || story.is_live {
            return false;
        }
        self.can_edit_stories(env, story_full_id.get_dialog_id())
    }

    /// `can_delete_story` (StoryManager.cpp:2354-2360).
    fn can_delete_story(
        &self,
        env: &impl StoryManagerEnv,
        story_full_id: StoryFullId,
        story: &Story,
    ) -> bool {
        if !story_full_id.get_story_id().is_server() {
            return true;
        }
        let owner_dialog_id = story_full_id.get_dialog_id();
        self.can_delete_stories(env, owner_dialog_id)
            || (story.is_outgoing && self.can_post_stories(env, owner_dialog_id))
    }

    /// `is_active_story` (StoryManager.cpp:2362-2364).
    fn is_active_story(&self, env: &mut impl StoryManagerEnv, story: &Story) -> bool {
        env.unix_time() < story.expire_date
    }

    /// `can_manage_story_albums` (StoryManager.cpp:2366-2372).
    fn can_manage_story_albums(
        &self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        source: &str,
    ) -> Status {
        let mut access = env.check_dialog_access(owner_dialog_id, false, false, source);
        if access.is_error() {
            return access.move_as_error();
        }
        if !self.can_edit_stories(env, owner_dialog_id) {
            return Status::error(400, "Can't manage story albums of the chat");
        }
        Status::ok()
    }

    /// `get_story_viewers_expire_date` (StoryManager.cpp:2374-2377).
    /// Cross-manager accessor (the reactions/views planes) — [blocked:
    /// Td update sends].
    #[allow(dead_code)]
    pub(crate) fn get_story_viewers_expire_date(
        &self,
        env: &mut impl StoryManagerEnv,
        story: &Story,
    ) -> i32 {
        story
            .expire_date
            .saturating_add(env.get_option_integer("story_viewers_expiration_delay", 86400) as i32)
    }

    /// `get_story` (StoryManager.cpp:2379-2381).
    pub(crate) fn get_story(&self, story_full_id: StoryFullId) -> Option<&Story> {
        self.stories.get(&story_full_id)
    }

    /// `get_story_editable` (StoryManager.cpp:2383-2385).
    /// Borrow twin of the accessor plane — [blocked: Td update sends].
    #[allow(dead_code)]
    fn get_story_mut(&mut self, story_full_id: StoryFullId) -> Option<&mut Story> {
        self.stories.get_mut(&story_full_id)
    }

    /// `get_active_stories` (StoryManager.cpp:2514-2516).
    /// Cross-manager accessor — [blocked: Td update sends].
    #[allow(dead_code)]
    pub(crate) fn get_active_stories_ref(
        &self,
        owner_dialog_id: DialogId,
    ) -> Option<&ActiveStories> {
        self.active_stories.get(&owner_dialog_id)
    }

    /// `get_active_stories_editable` (StoryManager.cpp:2518-2520).
    /// Borrow twin of the accessor plane — [blocked: Td update sends].
    #[allow(dead_code)]
    fn get_active_stories_mut(&mut self, owner_dialog_id: DialogId) -> Option<&mut ActiveStories> {
        self.active_stories.get_mut(&owner_dialog_id)
    }

    /// `get_active_stories_force` (StoryManager.cpp:2522-2540).
    fn get_active_stories_force(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        source: &str,
    ) -> Option<()>
    where
        Self: Sized,
    {
        if self.active_stories.contains_key(&owner_dialog_id) {
            return Some(());
        }
        if env.is_bot()
            || !env.use_message_database()
            || self
                .failed_to_load_active_stories
                .contains(&owner_dialog_id)
            || !owner_dialog_id.is_valid()
        {
            return None;
        }

        tdutils::log_info!(
            "Trying to load active stories of {} from database from {source}",
            owner_dialog_id.get()
        );
        let value = match env.story_db() {
            Some(db) => db.get_active_stories(owner_dialog_id),
            None => return None,
        };
        match value {
            Result::Err(_) => {
                self.failed_to_load_active_stories.insert(owner_dialog_id);
                None
            }
            Result::Ok(value) => {
                self.on_get_active_stories_from_database(
                    env,
                    StoryListId::None,
                    owner_dialog_id,
                    &value,
                    source,
                );
                if self.active_stories.contains_key(&owner_dialog_id) {
                    Some(())
                } else {
                    None
                }
            }
        }
    }

    /// `on_get_active_stories_from_database`
    /// (StoryManager.cpp:2542-2590).
    fn on_get_active_stories_from_database(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_list_id: StoryListId,
        owner_dialog_id: DialogId,
        value: &[u8],
        source: &str,
    ) -> Option<()> {
        let _ = source;
        if self.active_stories.contains_key(&owner_dialog_id) {
            return Some(());
        }
        if value.is_empty() {
            self.failed_to_load_active_stories.insert(owner_dialog_id);
            return None;
        }

        let mut saved_active_stories = match SavedActiveStories::parse(&mut Parser::new(value)) {
            Ok(saved) => saved,
            Err(status) => {
                tdutils::log_error!(
                    "Receive invalid active stories in {} from database: {status}",
                    owner_dialog_id.get()
                );
                self.save_active_stories(
                    env,
                    owner_dialog_id,
                    None,
                    Promise::default(),
                    "on_get_active_stories_from_database",
                );
                self.failed_to_load_active_stories.insert(owner_dialog_id);
                return None;
            }
        };

        let mut story_ids = Vec::with_capacity(saved_active_stories.story_infos.len());
        for story_info in std::mem::take(&mut saved_active_stories.story_infos) {
            story_ids.push(self.on_get_story_info(env, owner_dialog_id, story_info));
        }

        self.on_update_active_stories(
            env,
            owner_dialog_id,
            saved_active_stories.max_read_story_id,
            story_ids,
            Promise::default(),
            "on_get_active_stories_from_database",
            true,
        );

        if self.active_stories.contains_key(&owner_dialog_id) {
            return Some(());
        }
        let mut story_list_id = story_list_id;
        if !story_list_id.is_valid() {
            story_list_id = self.get_dialog_story_list_id(env, owner_dialog_id);
        }
        if story_list_id.is_valid() {
            let story_list = self.get_story_list_mut(story_list_id);
            if !story_list.is_reloaded_server_total_count
                && story_list.server_total_count > story_list.ordered_stories.len() as i32
            {
                story_list.server_total_count -= 1;
                let (state, server_total_count, server_has_more) = (
                    story_list.state.clone(),
                    story_list.server_total_count,
                    story_list.server_has_more,
                );
                self.update_story_list_sent_total_count(
                    env,
                    story_list_id,
                    "on_get_active_stories_from_database",
                );
                self.save_story_list(
                    env,
                    story_list_id,
                    state,
                    server_total_count,
                    server_has_more,
                );
            }
        }
        Some(())
    }

    /// `get_dialog_story_list_id` (StoryManager.cpp:5484-5505).
    fn get_dialog_story_list_id(
        &self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
    ) -> StoryListId {
        if !self.is_subscribed_to_dialog_stories(env, owner_dialog_id) {
            return StoryListId::None;
        }
        match owner_dialog_id.get_type() {
            DialogType::User => {
                if !self.is_my_story(env, owner_dialog_id)
                    && env.get_user_stories_hidden(owner_dialog_id.get_user_id())
                {
                    return StoryListId::Archive;
                }
                StoryListId::Main
            }
            DialogType::Channel => {
                if self.get_channel_stories_hidden(env, owner_dialog_id.get_channel_id().get()) {
                    return StoryListId::Archive;
                }
                StoryListId::Main
            }
            _ => StoryListId::Archive,
        }
    }

    /// `ChatManager::get_channel_stories_hidden` — [blocked: channel
    /// stories]; the interim answer is `false` (main list).
    fn get_channel_stories_hidden(&self, env: &impl StoryManagerEnv, channel_id: i64) -> bool {
        let _ = (self, env, channel_id);
        false
    }

    /// `get_changelog_story_dialog_id` (StoryManager.cpp:5450-5453).
    pub(crate) fn get_changelog_story_dialog_id(&self, env: &mut impl StoryManagerEnv) -> DialogId {
        DialogId::new(env.get_option_integer(
            "stories_changelog_user_id",
            crate::user_manager::UserManager::get_service_notifications_user_id().get(),
        ))
    }

    /// `is_subscribed_to_dialog_stories` (StoryManager.cpp:5455-5482).
    fn is_subscribed_to_dialog_stories(
        &self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
    ) -> bool {
        if env.is_bot() {
            return false;
        }
        if owner_dialog_id == self.get_changelog_story_dialog_id_ref(env) {
            return true;
        }
        match owner_dialog_id.get_type() {
            DialogType::User => {
                if self.is_my_story(env, owner_dialog_id) {
                    return true;
                }
                if env.is_user_contact(owner_dialog_id.get_user_id()) {
                    return true;
                }
                // `TopDialogManager::get_story_dialog_ids` — [blocked:
                // TopDialogManager]; the interim subscription set is the
                // contact/changelog checks.
                false
            }
            DialogType::Channel => env
                .get_channel_status(owner_dialog_id.get_channel_id().get())
                .is_member(),
            _ => false,
        }
    }

    /// The env-free changelog lookup for the borrow-split subscription
    /// check.
    fn get_changelog_story_dialog_id_ref(&self, env: &mut impl StoryManagerEnv) -> DialogId {
        DialogId::new(env.get_option_integer(
            "stories_changelog_user_id",
            crate::user_manager::UserManager::get_service_notifications_user_id().get(),
        ))
    }

    /// `get_story_list` (StoryManager.cpp:2879-2889).
    fn get_story_list_mut(&mut self, story_list_id: StoryListId) -> &mut StoryList {
        &mut self.story_lists[usize::from(story_list_id == StoryListId::Archive)]
    }

    fn get_story_list(&self, story_list_id: StoryListId) -> &StoryList {
        &self.story_lists[usize::from(story_list_id == StoryListId::Archive)]
    }

    /// `have_story` (StoryManager.cpp:3988-3990).
    pub fn have_story(&self, story_full_id: StoryFullId) -> bool {
        self.get_story(story_full_id).is_some()
    }

    /// `have_story_force` (StoryManager.cpp:3992-3994) — the db-backed
    /// force load.
    pub fn have_story_force(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
    ) -> bool {
        self.get_story_force(env, story_full_id, "have_story_force")
            .is_some()
    }

    /// `StoryManager::can_get_story_statistics` (StoryManager.cpp:
    /// 2498-2508) — the bot/live/server bands over the chat-manager
    /// statistics predicate (the env seam rides the channel read).
    pub fn can_get_story_statistics(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
    ) -> bool {
        let Some(story) = self.get_story_force(env, story_full_id, "can_get_story_statistics")
        else {
            return false;
        };
        if env.is_bot() || !story_full_id.get_story_id().is_server() || story.is_live {
            return false;
        }
        let dialog_id = story_full_id.get_dialog_id();
        dialog_id.get_type() == crate::dialog_id::DialogType::Channel
            && env.can_get_channel_story_statistics(dialog_id.get_channel_id().get())
    }

    /// `get_story_force` (StoryManager.cpp:2387-2411).
    pub(crate) fn get_story_force(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
        source: &str,
    ) -> Option<&Story> {
        if !story_full_id.is_valid() {
            return None;
        }
        if self
            .stories
            .get(&story_full_id)
            .is_some_and(|story| story.content.is_some())
        {
            return self.stories.get(&story_full_id);
        }

        if !env.use_message_database()
            || self.failed_to_load_story_full_ids.contains(&story_full_id)
            || self.is_inaccessible_story(story_full_id)
            || self.deleted_story_full_ids.contains(&story_full_id)
            || !story_full_id.get_story_id().is_server()
        {
            return None;
        }

        tdutils::log_info!("Trying to load {story_full_id} from database from {source}");
        let value = match env.story_db() {
            Some(db) => db.get_story(story_full_id),
            None => return None,
        };
        match value {
            Result::Err(_) => {
                self.failed_to_load_story_full_ids.insert(story_full_id);
                None
            }
            Result::Ok(value) => {
                self.on_get_story_from_database(env, story_full_id, &value, source);
                // the borrow lives only over the insert; re-read
                self.stories
                    .get(&story_full_id)
                    .filter(|story| story.content.is_some())
            }
        }
    }

    /// `parse_story` (StoryManager.cpp:2413-2456).
    fn parse_story(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
        value: &[u8],
    ) -> Option<Story> {
        let mut story = match Story::parse(env, &mut Parser::new(value)) {
            Ok(story) => story,
            Err(status) => {
                tdutils::log_error!("Receive invalid {story_full_id} from database: {status}");
                self.delete_story_from_database(env, story_full_id);
                self.reload_story(env, story_full_id, Promise::default(), "parse_story");
                return None;
            }
        };
        if story.content.is_none() {
            tdutils::log_error!("Receive {story_full_id} without content from database");
            self.delete_story_from_database(env, story_full_id);
            return None;
        }
        if !story_full_id.get_story_id().is_server() {
            tdutils::log_error!("Receive {story_full_id} from database");
            self.delete_story_from_database(env, story_full_id);
            return None;
        }

        let owner_dialog_id = story_full_id.get_dialog_id();
        if self.is_active_story(env, &story) {
            if let Some(active_stories) = self.active_stories.get(&owner_dialog_id)
                && !active_stories
                    .story_ids
                    .contains(&story_full_id.get_story_id())
            {
                tdutils::log_info!("Ignore unavailable active {story_full_id} from database");
                self.delete_story_from_database(env, story_full_id);
                return None;
            }
        } else if !self.can_access_expired_story(env, owner_dialog_id, &story) {
            tdutils::log_info!("Delete expired {story_full_id}");
            self.delete_story_from_database(env, story_full_id);
            return None;
        }
        if self.is_my_story(env, owner_dialog_id) {
            story.is_outgoing = true;
        }

        Some(story)
    }

    /// `on_get_story_from_database` (StoryManager.cpp:2458-2496).
    fn on_get_story_from_database(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
        value: &[u8],
        source: &str,
    ) -> Option<()> {
        if let Some(old_story) = self.stories.get(&story_full_id)
            && old_story.content.is_some()
        {
            return Some(());
        }
        if value.is_empty() {
            self.failed_to_load_story_full_ids.insert(story_full_id);
            return None;
        }

        let mut story = match self.parse_story(env, story_full_id, value) {
            Some(story) => story,
            None => {
                self.failed_to_load_story_full_ids.insert(story_full_id);
                return None;
            }
        };

        // the dependency resolve is the force-create seam
        if let Some(forward_info) = &story.forward_info {
            forward_info.add_dependencies(env);
        }
        story.interaction_info.add_dependencies(env);
        if story.sender_dialog_id.is_valid() {
            env.force_create_dialog(story.sender_dialog_id, "on_get_story_from_database", true);
        }

        tdutils::log_info!("Load new {story_full_id} from {source}");

        story.global_id = 0;
        let result = story;
        self.stories.insert(story_full_id, result);
        self.register_story_global_id(story_full_id);

        let story = self
            .stories
            .get(&story_full_id)
            .cloned()
            .expect("just inserted");
        self.on_story_changed(env, story_full_id, &story, true, false, true);
        Some(())
    }

    /// `register_story_global_id` (StoryManager.cpp:4829-4834).
    fn register_story_global_id(&mut self, story_full_id: StoryFullId) {
        self.max_story_global_id += 1;
        let global_id = self.max_story_global_id;
        if let Some(story) = self.stories.get_mut(&story_full_id) {
            story.global_id = global_id;
        }
        self.stories_by_global_id.insert(global_id, story_full_id);
    }

    /// `unregister_story_global_id` (StoryManager.cpp:4836-4839).
    fn unregister_story_global_id(&mut self, story_full_id: StoryFullId) {
        if let Some(story) = self.stories.get(&story_full_id) {
            self.stories_by_global_id.remove(&story.global_id);
        }
    }

    /// `is_inaccessible_story` (StoryManager.cpp:4001-4003).
    pub fn is_inaccessible_story(&self, story_full_id: StoryFullId) -> bool {
        self.inaccessible_story_full_ids
            .contains_key(&story_full_id)
    }

    /// `get_story_date` (StoryManager.cpp:3996-3999).
    pub fn get_story_date(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
    ) -> i32 {
        self.get_story_force(env, story_full_id, "get_story_date")
            .map(|story| story.date)
            .unwrap_or(0)
    }

    /// `get_story_duration` (StoryManager.cpp:4005-4018).
    pub fn get_story_duration(
        &self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
    ) -> i32 {
        let Some(story) = self.stories.get(&story_full_id) else {
            return -1;
        };
        let mut content = story.content.as_ref();
        if content.is_none() {
            return -1;
        }
        if let Some(edited_story) = self.being_edited_stories.get(&story_full_id)
            && edited_story.content.is_some()
        {
            content = edited_story.content.as_ref();
        }
        content
            .map(|content| get_story_content_duration(env, content))
            .unwrap_or(-1)
    }

    /// `delete_story_from_database` (StoryManager.cpp:4747-4752).
    fn delete_story_from_database(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
    ) {
        if env.use_message_database() {
            tdutils::log_info!("Delete {story_full_id} from database");
            if let Some(db) = env.story_db() {
                let _ = db.delete_story(story_full_id);
            }
        }
    }

    /// `on_update_story_stealth_mode` (StoryManager.cpp:5369-5372).
    pub fn on_update_story_stealth_mode(
        &mut self,
        env: &mut impl StoryManagerEnv,
        stealth_mode: Option<telegram_api::StoriesStealthMode>,
    ) {
        let Some(stealth_mode) = stealth_mode else {
            return;
        };
        let unix_time = env.unix_time();
        self.set_story_stealth_mode(
            env,
            StoryStealthMode::from_telegram(stealth_mode, unix_time),
        );
    }

    /// `set_story_stealth_mode` (StoryManager.cpp:5415-5431).
    pub fn set_story_stealth_mode(
        &mut self,
        env: &mut impl StoryManagerEnv,
        mut stealth_mode: StoryStealthMode,
    ) {
        stealth_mode.update(env.unix_time());
        if stealth_mode == self.stealth_mode {
            return;
        }

        self.stealth_mode = stealth_mode;
        tdutils::log_info!("{stealth_mode}");
        self.send_update_story_stealth_mode(env);

        if self.stealth_mode.is_empty() {
            env.binlog_pmc_erase(Self::get_story_stealth_mode_key().as_bytes());
        } else {
            let mut out = Vec::new();
            // `log_event_store` framing — the version prefix leads the row.
            crate::store_parse::store_log_event_version(&mut out);
            self.stealth_mode.store(&mut out);
            env.binlog_pmc_set_raw(Self::get_story_stealth_mode_key().as_bytes(), &out);
        }
    }

    /// `get_update_story_stealth_mode` (StoryManager.cpp:5358-5360).
    pub fn get_update_story_stealth_mode(&self) -> td_api::Update {
        self.stealth_mode.get_update_story_stealth_mode_object()
    }

    /// `send_update_story_stealth_mode` (StoryManager.cpp:5362-5367).
    pub fn send_update_story_stealth_mode(&self, env: &mut impl StoryManagerEnv) {
        if env.is_bot() {
            return;
        }
        env.send_update(self.get_update_story_stealth_mode());
    }

    /// `update_stealth_mode` (StoryManager.cpp:5442-5448) — the timeout
    /// driver: the port runs it from the test/flush hooks.
    pub fn update_stealth_mode(&mut self, env: &mut impl StoryManagerEnv) {
        if self.stealth_mode.update(env.unix_time()) {
            tdutils::log_info!("{}", self.stealth_mode);
            self.send_update_story_stealth_mode(env);
        }
    }

    /// `save_story_list` (StoryManager.cpp:2866-2877).
    fn save_story_list(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_list_id: StoryListId,
        state: String,
        total_count: i32,
        has_more: bool,
    ) {
        if !env.use_message_database() {
            return;
        }
        let saved_story_list = SavedStoryList {
            state,
            total_count,
            has_more,
        };
        let mut out = Vec::new();
        saved_story_list.store(&mut out);
        if let Some(db) = env.story_db() {
            let _ = db.add_active_story_list_state(story_list_id, &out);
        }
    }

    /// `update_story_list_sent_total_count` (StoryManager.cpp:2905-2929).
    fn update_story_list_sent_total_count(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_list_id: StoryListId,
        source: &str,
    ) {
        let mut yet_unsent_total_count = 0;
        for dialog_id in self.yet_unsent_story_ids.keys() {
            if !self.active_stories.contains_key(dialog_id) {
                yet_unsent_total_count += 1;
            }
        }
        let story_list = self.get_story_list_mut(story_list_id);
        if story_list.server_total_count == -1 || env.is_bot() {
            return;
        }
        tdutils::log_info!("Update story list sent total chat count from {source}");
        let mut new_total_count = story_list.ordered_stories.len() as i32;
        new_total_count += yet_unsent_total_count;
        if story_list.list_last_story_date != DialogDate::max_dialog_date() {
            new_total_count =
                new_total_count.max(story_list.server_total_count + yet_unsent_total_count);
        } else if story_list.server_total_count != new_total_count {
            story_list.server_total_count = new_total_count;
        }
        let (state, server_total_count, server_has_more, sent_total_count) = {
            let story_list = self.get_story_list_mut(story_list_id);
            (
                story_list.state.clone(),
                story_list.server_total_count,
                story_list.server_has_more,
                story_list.sent_total_count,
            )
        };
        if server_total_count != new_total_count {
            self.save_story_list(
                env,
                story_list_id,
                state,
                server_total_count,
                server_has_more,
            );
        }
        let mut sent_total_count = sent_total_count;
        if sent_total_count != new_total_count {
            sent_total_count = new_total_count;
            self.get_story_list_mut(story_list_id).sent_total_count = new_total_count;
            env.send_update(td_api::Update::updateStoryListChatCount(
                td_api::updateStoryListChatCount {
                    story_list: story_list_id.get_story_list_object(),
                    chat_count: sent_total_count,
                },
            ));
        }
    }

    /// `get_update_story_list_chat_count_object`
    /// (StoryManager.cpp:2891-2896).
    fn get_update_story_list_chat_count_object(
        &self,
        story_list_id: StoryListId,
        story_list: &StoryList,
    ) -> td_api::Update {
        td_api::Update::updateStoryListChatCount(td_api::updateStoryListChatCount {
            story_list: story_list_id.get_story_list_object(),
            chat_count: story_list.sent_total_count,
        })
    }

    /// `StoryManager::get_story_info` (StoryManager.cpp:4099-4118).
    fn get_story_info(&self, story_full_id: StoryFullId) -> StoryInfo {
        if let Some(story) = self.get_story(story_full_id) {
            return StoryInfo {
                story_id: story_full_id.get_story_id(),
                date: story.date,
                expire_date: story.expire_date,
                is_for_close_friends: story.is_for_close_friends,
                is_live: story.is_live,
            };
        }
        StoryInfo::default()
    }

    /// `StoryManager::is_story_live` (StoryManager.cpp:4119-4123).
    fn is_story_live(&self, story_full_id: StoryFullId) -> bool {
        self.get_story(story_full_id)
            .is_some_and(|story| story.is_live)
    }

    /// `StoryManager::get_story_info_object` (StoryManager.cpp:4124-4132).
    fn get_story_info_object(&self, story_full_id: StoryFullId) -> Option<td_api::StoryInfo> {
        if !story_full_id.get_story_id().is_server() {
            return None;
        }
        let story_info = self.get_story_info(story_full_id);
        if !story_info.story_id.is_server() || story_info.date <= 0 {
            return None;
        }
        Some(td_api::StoryInfo {
            story_id: story_info.story_id.get(),
            date: story_info.date,
            is_for_close_friends: story_info.is_for_close_friends,
            is_live: story_info.is_live,
        })
    }

    /// `StoryManager::get_story_object` (StoryManager.cpp:4133-4135).
    pub fn get_story_object(
        &self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
    ) -> Option<td_api::Story> {
        let story = self.get_story(story_full_id)?;
        self.get_story_object_for(env, story_full_id, story)
    }

    /// `StoryManager::get_story_object(StoryFullId, const Story *)`
    /// (StoryManager.cpp:4137-4226).
    fn get_story_object_for(
        &self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
        story: &Story,
    ) -> Option<td_api::Story> {
        if story_full_id.get_story_id().is_server() && story.date <= 0 {
            tdutils::log_error!("Receive {story_full_id} sent at {}", story.date);
            return None;
        }
        let content = story.content.as_ref()?;
        let owner_dialog_id = story_full_id.get_dialog_id();

        let is_editable = self.can_edit_story_ref(env, story_full_id, Some(story));
        let can_be_deleted = story_full_id.get_story_id().is_server()
            && self.can_delete_story_ref(env, story_full_id, story);
        let is_my_story = self.is_my_story(env, owner_dialog_id);
        let privacy_settings = if is_my_story || env.is_bot() {
            story.privacy_rules.get_story_privacy_settings_object(env)
        } else {
            None
        };

        Some(td_api::Story {
            id: story_full_id.get_story_id().get(),
            poster_chat_id: env.get_chat_id_object(owner_dialog_id, "get_story_object"),
            poster_id: if story.sender_dialog_id.is_valid() {
                env.get_message_sender_object(story.sender_dialog_id, "get_story_object")
            } else {
                None
            },
            date: story.date,
            is_being_posted: false,
            is_being_edited: self.being_edited_stories.contains_key(&story_full_id),
            is_edited: story.is_edited,
            is_posted_to_chat_page: story.is_pinned,
            is_visible_only_for_self: story.is_for_close_friends
                || story.is_for_contacts
                || story.is_for_selected_contacts,
            can_be_added_to_album: is_editable
                && story_full_id.get_story_id().is_server()
                && !story.is_live
                && self.can_add_story_to_album_ref(env, story_full_id, story),
            can_be_deleted,
            can_be_edited: is_editable,
            can_be_forwarded: !story.noforwards,
            can_be_replied: !story.noforwards && !is_my_story && !story.is_live,
            can_set_privacy_settings: is_my_story
                && story_full_id.get_story_id().is_server()
                && self.can_set_story_privacy_settings_ref(env, story_full_id, story),
            can_toggle_is_posted_to_chat_page: is_editable
                && story_full_id.get_story_id().is_server()
                && (story.is_pinned || !story.is_live),
            can_get_statistics: false,
            can_get_interactions: false,
            has_expired_viewers: story.interaction_info.has_hidden_viewers()
                && env.unix_time() >= self.get_story_viewers_expire_date_ref(env, story),
            repost_info: story
                .forward_info
                .as_ref()
                .map(|forward_info| forward_info.get_story_repost_info_object(env)),
            interaction_info: story
                .interaction_info
                .get_story_interaction_info_object(env),
            chosen_reaction_type: story.chosen_reaction_type.get_reaction_type_object(),
            privacy_settings,
            content: Some(get_story_content_object(env, content, owner_dialog_id)),
            areas: story.areas.iter().map(|area| area.area.clone()).collect(),
            caption: if !story.caption.text.is_empty() {
                Some(crate::message_entity::get_formatted_text_object(
                    Some(&mut ()),
                    &story.caption,
                    true,
                    i32::MAX,
                ))
            } else {
                None
            },
            // The story music (StoryManager.cpp:4474-4483 — the wire
            // music document → audio object) rides the story-music
            // follow-up.
            music: None,
            album_ids: StoryAlbumId::get_story_album_ids_object(&story.album_ids),
        })
    }

    /// The `&self` mirrors of the can_* checks for the object builders.
    fn can_edit_story_ref(
        &self,
        env: &impl StoryManagerEnv,
        story_full_id: StoryFullId,
        story: Option<&Story>,
    ) -> bool {
        if !story_full_id.get_story_id().is_server() {
            return false;
        }
        if let Some(story) = story
            && let Some(content) = &story.content
            && !can_edit_story_content(content.get_type())
        {
            return false;
        }
        let owner_dialog_id = story_full_id.get_dialog_id();
        self.can_edit_stories(env, owner_dialog_id)
            || (story.is_some_and(|story| story.is_outgoing)
                && self.can_post_stories(env, owner_dialog_id))
    }

    fn can_delete_story_ref(
        &self,
        env: &impl StoryManagerEnv,
        story_full_id: StoryFullId,
        story: &Story,
    ) -> bool {
        if !story_full_id.get_story_id().is_server() {
            return true;
        }
        let owner_dialog_id = story_full_id.get_dialog_id();
        self.can_delete_stories(env, owner_dialog_id)
            || (story.is_outgoing && self.can_post_stories(env, owner_dialog_id))
    }

    fn can_add_story_to_album_ref(
        &self,
        env: &impl StoryManagerEnv,
        story_full_id: StoryFullId,
        story: &Story,
    ) -> bool {
        if !story_full_id.get_story_id().is_server() || story.is_live {
            return false;
        }
        self.can_edit_stories(env, story_full_id.get_dialog_id())
    }

    fn can_set_story_privacy_settings_ref(
        &self,
        env: &impl StoryManagerEnv,
        story_full_id: StoryFullId,
        story: &Story,
    ) -> bool {
        if !story_full_id.get_story_id().is_server() {
            return false;
        }
        let owner_dialog_id = story_full_id.get_dialog_id();
        self.can_edit_stories(env, owner_dialog_id)
            || (story.is_outgoing && self.can_post_stories(env, owner_dialog_id))
    }

    fn get_story_viewers_expire_date_ref(
        &self,
        env: &mut impl StoryManagerEnv,
        story: &Story,
    ) -> i32 {
        story
            .expire_date
            .saturating_add(env.get_option_integer("story_viewers_expiration_delay", 86400) as i32)
    }

    /// `StoryManager::get_stories_object` (StoryManager.cpp:4227-4238).
    pub fn get_stories_object(
        &self,
        env: &mut impl StoryManagerEnv,
        total_count: i32,
        story_full_ids: &[StoryFullId],
        pinned_story_ids: &[StoryId],
    ) -> td_api::Stories {
        let stories: Vec<td_api::Story> = story_full_ids
            .iter()
            .filter_map(|story_full_id| self.get_story_object(env, *story_full_id))
            .collect();
        td_api::Stories {
            total_count,
            stories,
            pinned_story_ids: StoryId::get_input_story_ids(pinned_story_ids),
        }
    }

    /// `StoryManager::get_chat_active_stories_object`
    /// (StoryManager.cpp:4239-4277).
    fn get_chat_active_stories_object(
        &self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        active_stories: Option<&ActiveStories>,
    ) -> td_api::ChatActiveStories {
        let Some(active_stories) = active_stories else {
            return td_api::ChatActiveStories::default();
        };
        td_api::ChatActiveStories {
            chat_id: env.get_chat_id_object(owner_dialog_id, "get_chat_active_stories_object"),
            list: active_stories.story_list_id.get_story_list_object(),
            order: if active_stories.story_list_id.is_valid() {
                active_stories.public_order
            } else {
                0
            },
            can_be_archived: active_stories.can_be_archived,
            max_read_story_id: active_stories.max_read_story_id.get(),
            stories: active_stories
                .story_ids
                .iter()
                .filter_map(|story_id| {
                    self.get_story_info_object(StoryFullId::new(owner_dialog_id, *story_id))
                })
                .collect(),
        }
    }

    /// `StoryManager::get_can_post_story_result_object`
    /// (StoryManager.cpp:4279-4321) — the Status → CanPostStoryResult
    /// collapse.
    pub fn get_can_post_story_result_object(
        error: &Status,
        force: bool,
    ) -> td_api::CanPostStoryResult {
        if error.code() == 429 {
            return td_api::CanPostStoryResult::canPostStoryResultActiveStoryLimitExceeded(
                td_api::canPostStoryResultActiveStoryLimitExceeded,
            );
        }
        let message = error.message();
        if message == "STORY_SEND_ACTIVE_STORY_LIMIT_EXCEEDED" {
            return td_api::CanPostStoryResult::canPostStoryResultActiveStoryLimitExceeded(
                td_api::canPostStoryResultActiveStoryLimitExceeded,
            );
        }
        if message == "STORY_SEND_WEEKLY_LIMIT_EXCEEDED" {
            return td_api::CanPostStoryResult::canPostStoryResultWeeklyLimitExceeded(
                td_api::canPostStoryResultWeeklyLimitExceeded::default(),
            );
        }
        if message == "STORY_SEND_MONTHLY_LIMIT_EXCEEDED" {
            return td_api::CanPostStoryResult::canPostStoryResultMonthlyLimitExceeded(
                td_api::canPostStoryResultMonthlyLimitExceeded::default(),
            );
        }
        if message == "PREMIUM_ACCOUNT_REQUIRED" {
            return td_api::CanPostStoryResult::canPostStoryResultPremiumNeeded(
                td_api::canPostStoryResultPremiumNeeded,
            );
        }
        if message == "BOOST_REQUIRED" || (force && error.code() == 429) {
            return td_api::CanPostStoryResult::canPostStoryResultBoostNeeded(
                td_api::canPostStoryResultBoostNeeded,
            );
        }
        let _ = force;
        td_api::CanPostStoryResult::canPostStoryResultOk(td_api::canPostStoryResultOk::default())
    }

    /// `StoryManager::send_update_story` (StoryManager.cpp:5226-5235).
    fn send_update_story(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
        story: &Story,
    ) {
        let Some(story_object) = self.get_story_object_for(env, story_full_id, story) else {
            // the story can be just expired
            return;
        };
        env.send_update(td_api::Update::updateStory(td_api::updateStory {
            story: Some(story_object),
        }));
    }

    /// `StoryManager::on_story_changed` (StoryManager.cpp:4768-4827) —
    /// the expire/viewers timeouts are the [blocked: Timeout actor] notes;
    /// the persistence and the update fan-out are real.
    pub fn on_story_changed(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
        story: &Story,
        is_changed: bool,
        need_save_to_database: bool,
        from_database: bool,
    ) {
        if !story_full_id.get_story_id().is_server() {
            return;
        }
        if self.is_active_story(env, story) {
            // `set_story_expire_timeout` — [blocked: Timeout actor]; the
            // expiry decision runs from `load_expired_database_stories`
        }
        if self.has_unexpired_viewers(env, story_full_id, story) {
            // `set_story_can_get_viewers_timeout` — [blocked: Timeout actor]
        }
        let Some(content) = &story.content else {
            return;
        };
        if is_changed || need_save_to_database {
            if env.use_message_database() && !from_database {
                tdutils::log_info!("Add {story_full_id} to database");

                let mut expires_at = 0;
                if self.is_active_story(env, story)
                    && !self.can_access_expired_story(env, story_full_id.get_dialog_id(), story)
                {
                    expires_at = story.expire_date;
                }

                let mut out = Vec::new();
                story.store(env, &mut out);
                if let Some(db) = env.story_db() {
                    let _ =
                        db.add_story(story_full_id, expires_at, NotificationId::default(), &out);
                }
            }

            if is_changed && story.is_update_sent {
                self.send_update_story(env, story_full_id, story);
            }

            // the MessagesManager/WebPagesManager/QuickReplyManager
            // external-update hops are [blocked: their tracks]
            let _ = content;
        }
    }

    /// `StoryManager::get_update_chat_active_stories_object`
    /// (StoryManager.cpp:5237-5241).
    fn get_update_chat_active_stories_object(
        &self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        active_stories: Option<&ActiveStories>,
    ) -> td_api::Update {
        td_api::Update::updateChatActiveStories(td_api::updateChatActiveStories {
            active_stories: Some(self.get_chat_active_stories_object(
                env,
                owner_dialog_id,
                active_stories,
            )),
        })
    }

    /// `StoryManager::send_update_chat_active_stories`
    /// (StoryManager.cpp:5243-5258).
    fn send_update_chat_active_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        active_stories: Option<&ActiveStories>,
        source: &str,
    ) {
        if env.is_bot() {
            return;
        }
        if !self.updated_active_stories.contains(&owner_dialog_id) {
            if active_stories.is_none_or(|active_stories| active_stories.public_order == 0) {
                tdutils::log_info!("Skip update about active stories from {source}");
                return;
            }
            self.updated_active_stories.insert(owner_dialog_id);
        }
        tdutils::log_info!("Send update about active stories from {source}");
        let update =
            self.get_update_chat_active_stories_object(env, owner_dialog_id, active_stories);
        env.send_update(update);
    }

    /// `StoryManager::save_active_stories` (StoryManager.cpp:5260-5291).
    fn save_active_stories(
        &self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        active_stories: Option<&ActiveStories>,
        promise: Promise<()>,
        source: &str,
    ) {
        if !env.use_message_database() {
            let mut promise = promise;
            promise.set_value(());
            return;
        }
        let Some(active_stories) = active_stories else {
            tdutils::log_info!(
                "Delete active stories of {} from database from {source}",
                owner_dialog_id.get()
            );
            if let Some(db) = env.story_db() {
                let _ = db.delete_active_stories(owner_dialog_id);
            }
            let mut promise = promise;
            promise.set_value(());
            return;
        };
        tdutils::log_info!(
            "Add {} active stories of {} to database from {source}",
            active_stories.story_ids.len(),
            owner_dialog_id.get()
        );
        let order = if active_stories.story_list_id.is_valid() {
            active_stories.private_order
        } else {
            0
        };
        let mut saved_active_stories = SavedActiveStories {
            max_read_story_id: active_stories.max_read_story_id,
            story_infos: Vec::new(),
        };
        for story_id in &active_stories.story_ids {
            let story_info = self.get_story_info(StoryFullId::new(owner_dialog_id, *story_id));
            if story_info.story_id.is_valid() {
                saved_active_stories.story_infos.push(story_info);
            }
        }
        if saved_active_stories.story_infos.is_empty() {
            tdutils::log_info!("Have no active stories to save");
            if let Some(db) = env.story_db() {
                let _ = db.delete_active_stories(owner_dialog_id);
            }
        } else {
            let mut out = Vec::new();
            saved_active_stories.store(&mut out);
            if let Some(db) = env.story_db() {
                let _ = db.add_active_stories(
                    owner_dialog_id,
                    active_stories.story_list_id,
                    order,
                    &out,
                );
            }
        }
        let mut promise = promise;
        promise.set_value(());
    }

    /// `StoryManager::add_story_dependencies` (StoryManager.cpp:2592-2606).
    fn add_story_dependencies(&mut self, env: &mut impl StoryManagerEnv, story: &Story) {
        if let Some(forward_info) = &story.forward_info {
            forward_info.add_dependencies(env);
        }
        story.interaction_info.add_dependencies(env);
        if story.sender_dialog_id.is_valid() {
            env.force_create_dialog(story.sender_dialog_id, "add_story_dependencies", true);
        }
        // `story->privacy_rules_.add_dependencies` — the story-plane rules
        // carry user ids only
        for rule in &story.privacy_rules.rules {
            for user_id in &rule.user_ids {
                env.force_create_dialog(
                    DialogId::from_user_id(*user_id),
                    "add_story_dependencies",
                    true,
                );
            }
        }
    }

    /// `StoryManager::get_story_file_ids` (StoryManager.cpp:4322-4328).
    fn get_story_file_ids(&self, env: &mut impl StoryManagerEnv, story: &Story) -> Vec<FileId> {
        let Some(content) = &story.content else {
            return Vec::new();
        };
        get_story_content_file_ids(env, content)
    }

    /// `StoryManager::delete_story_files` (StoryManager.cpp:4329-4334) —
    /// the unpinned-file GC is [blocked: the file GC integration].
    fn delete_story_files(&mut self, env: &mut impl StoryManagerEnv, story: &Story) {
        let _ = self.get_story_file_ids(env, story);
        tdutils::log_info!("delete_story_files — [blocked: file GC integration]");
    }

    /// `StoryManager::change_story_files` (StoryManager.cpp:4335-4353) —
    /// the file-reference pin updates are [blocked: the file GC
    /// integration].
    fn change_story_files(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
        story: &Story,
        old_file_ids: &[FileId],
    ) {
        let _ = (story_full_id, story, old_file_ids, env);
        tdutils::log_info!("change_story_files — [blocked: file GC integration]");
    }

    /// `StoryManager::on_get_story` (StoryManager.cpp:4354-4374).
    pub fn on_get_story(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_item_ptr: Option<telegram_api::StoryItem>,
    ) -> StoryId {
        if !owner_dialog_id.is_valid() {
            tdutils::log_error!("Receive a story in dialog {}", owner_dialog_id.get());
            return StoryId::default();
        }
        let Some(story_item_ptr) = story_item_ptr else {
            return StoryId::default();
        };
        match story_item_ptr {
            telegram_api::StoryItem::storyItemDeleted(story_item) => {
                self.on_get_deleted_story(env, owner_dialog_id, StoryId::new(story_item.id));
                StoryId::new(story_item.id)
            }
            telegram_api::StoryItem::storyItemSkipped(story_item) => {
                tdutils::log_error!("Receive storyItemSkipped in on_get_story");
                self.on_get_skipped_story(env, owner_dialog_id, story_item)
            }
            telegram_api::StoryItem::storyItem(story_item) => {
                self.on_get_new_story(env, owner_dialog_id, Box::new(story_item))
            }
        }
    }

    /// `StoryManager::on_get_skipped_story` (StoryManager.cpp:4626-4636).
    fn on_get_skipped_story(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_item: telegram_api::storyItemSkipped,
    ) -> StoryId {
        let story_info = StoryInfo {
            story_id: StoryId::new(story_item.id),
            date: story_item.date,
            expire_date: story_item.expire_date,
            is_for_close_friends: story_item.close_friends,
            is_live: story_item.live,
        };
        self.on_get_story_info(env, owner_dialog_id, story_info)
    }

    /// `StoryManager::on_get_story_info` (StoryManager.cpp:4638-4682).
    fn on_get_story_info(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        mut story_info: StoryInfo,
    ) -> StoryId {
        let story_id = story_info.story_id;
        if !story_id.is_server() {
            tdutils::log_error!("Receive {story_id}");
            return StoryId::default();
        }
        if self
            .deleted_story_full_ids
            .contains(&StoryFullId::new(owner_dialog_id, story_id))
        {
            return StoryId::default();
        }

        env.force_create_dialog(owner_dialog_id, "on_get_story_info", false);

        let story_full_id = StoryFullId::new(owner_dialog_id, story_id);
        if !self.stories.contains_key(&story_full_id) {
            let story = Story {
                is_outgoing: self.is_my_story(env, owner_dialog_id),
                ..Story::default()
            };
            self.stories.insert(story_full_id, story);
            self.register_story_global_id(story_full_id);
            self.inaccessible_story_full_ids.remove(&story_full_id);
        }

        if story_info.date <= 0 {
            tdutils::log_error!("Receive {story_full_id} sent at {}", story_info.date);
            story_info.date = 1;
        }
        if story_info.expire_date <= story_info.date {
            tdutils::log_error!(
                "Receive {story_full_id} sent at {}, but expired at {}",
                story_info.date,
                story_info.expire_date
            );
            story_info.expire_date = story_info.date + 1;
        }

        let story = self.stories.get_mut(&story_full_id).expect("just inserted");
        if story.date != story_info.date
            || story.expire_date != story_info.expire_date
            || story.is_for_close_friends != story_info.is_for_close_friends
            || story.is_live != story_info.is_live
        {
            story.date = story_info.date;
            story.expire_date = story_info.expire_date;
            story.is_for_close_friends = story_info.is_for_close_friends;
            story.is_live = story_info.is_live;
            let story = story.clone();
            self.on_story_changed(env, story_full_id, &story, true, true, false);
        }
        story_id
    }

    /// `StoryManager::on_get_deleted_story` (StoryManager.cpp:4684-4689).
    fn on_get_deleted_story(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_id: StoryId,
    ) {
        self.on_delete_story(env, StoryFullId::new(owner_dialog_id, story_id));
    }

    /// `StoryManager::on_delete_story` (StoryManager.cpp:4691-4745).
    pub fn on_delete_story(&mut self, env: &mut impl StoryManagerEnv, story_full_id: StoryFullId) {
        let story_id = story_full_id.get_story_id();
        if !story_id.is_server() {
            tdutils::log_error!("Receive deleted {story_full_id}");
            return;
        }

        self.update_story_ids.remove(&story_full_id);

        let is_bot = env.is_bot();
        if !is_bot {
            self.inaccessible_story_full_ids
                .insert(story_full_id, env.unix_time() as f64);
            // `MessagesManager::update_story_max_reply_media_timestamp...`
            // — [blocked: MessagesManager]
        }

        let story = self
            .get_story_force(env, story_full_id, "on_delete_story")
            .cloned();
        let owner_dialog_id = story_full_id.get_dialog_id();
        if let Some(story) = story {
            tdutils::log_info!("Delete {story_full_id}");
            if story.is_update_sent {
                let poster_chat_id = env.get_chat_id_object(owner_dialog_id, "updateStoryDeleted");
                env.send_update(td_api::Update::updateStoryDeleted(
                    td_api::updateStoryDeleted {
                        story_poster_chat_id: poster_chat_id,
                        story_id: story_id.get(),
                    },
                ));
            }
            self.delete_story_files(env, &story);
            self.unregister_story_global_id(story_full_id);
            self.stories.remove(&story_full_id);
            if let Some(edited_story) = self.being_edited_stories.remove(&story_full_id) {
                // the binlog erase is the [blocked: binlog] seam
                let _ = edited_story.log_event_id;
            }
            self.edit_generations.remove(&story_full_id);
        } else {
            tdutils::log_info!("Delete not found {story_full_id}");
        }

        if !is_bot
            && let Some(active_stories) = self.active_stories.get(&owner_dialog_id)
            && active_stories.story_ids.contains(&story_id)
        {
            let mut story_ids = active_stories.story_ids.clone();
            story_ids.retain(|&story| story != story_id);
            let max_read_story_id = active_stories.max_read_story_id;
            self.on_update_active_stories(
                env,
                owner_dialog_id,
                max_read_story_id,
                story_ids,
                Promise::default(),
                "on_delete_story",
                false,
            );
        }

        self.delete_story_from_database(env, story_full_id);
    }

    /// `StoryManager::on_get_new_story` (StoryManager.cpp:4376-4624).
    fn on_get_new_story(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        mut story_item: Box<telegram_api::storyItem>,
    ) -> StoryId {
        let story_id = StoryId::new(story_item.id);
        if !story_id.is_server() {
            tdutils::log_error!("Receive invalid story item");
            return StoryId::default();
        }
        let story_full_id = StoryFullId::new(owner_dialog_id, story_id);
        if self.deleted_story_full_ids.contains(&story_full_id) {
            return StoryId::default();
        }

        let mut old_story_id = StoryId::default();
        if let Some(removed) = self.update_story_ids.remove(&story_full_id) {
            old_story_id = removed;
            tdutils::log_info!("Receive sent {old_story_id} as {story_full_id}");

            let old_story_full_id = StoryFullId::new(owner_dialog_id, old_story_id);
            match self.stories.remove(&old_story_full_id) {
                Some(old_story) => {
                    self.delete_story_files(env, &old_story);
                    self.unregister_story_global_id(old_story_full_id);
                }
                None => {
                    old_story_id = StoryId::default();
                }
            }
        }

        env.force_create_dialog(owner_dialog_id, "on_get_new_story", false);

        // the `get_message_text` telegram-entity intake is [blocked: the
        // message-text integration]; the interim caption keeps the text and
        // drops the server entities
        let caption = crate::message_entity::FormattedText {
            text: std::mem::take(&mut story_item.caption),
            entities: Vec::new(),
        };
        let media = story_item.media.take().map(|boxed| *boxed);
        let content = match media {
            Some(media) => get_story_content(env, media, owner_dialog_id, false),
            None => None,
        };
        let Some(mut new_content) = content else {
            tdutils::log_info!("Failed to get content of {story_full_id}");
            return StoryId::default();
        };

        if let std::collections::hash_map::Entry::Vacant(entry) = self.stories.entry(story_full_id)
        {
            entry.insert(Story::default());
            self.register_story_global_id(story_full_id);
            self.inaccessible_story_full_ids.remove(&story_full_id);
            self.failed_to_load_story_full_ids.remove(&story_full_id);
            story_item.min = false;
            tdutils::log_info!("Add new {story_full_id}");
        }

        let receive_date = env.unix_time();

        let content_type = new_content.get_type();
        let old_file_ids = {
            let story = self.stories.get(&story_full_id).expect("just inserted");
            self.get_story_file_ids(env, story)
        };
        let edited_state = self.being_edited_stories.get(&story_full_id).map(|edited| {
            (
                edited.content.is_some(),
                edited.edit_media_areas,
                edited.edit_caption,
            )
        });
        let mut is_changed = false;
        let mut need_save_to_database = false;
        {
            let story = self.stories.get_mut(&story_full_id).expect("just inserted");
            story.receive_date = receive_date;

            let has_edited_content = edited_state.is_some_and(|(has_content, _, _)| has_content);
            if has_edited_content {
                story.content = Some(new_content.clone());
                need_save_to_database = true;
            } else if story.content.is_none()
                || story.content.as_ref().unwrap().get_type() != content_type
            {
                story.content = Some(new_content.clone());
                is_changed = true;
            } else {
                let mut old_content = story.content.clone().expect("checked above");
                merge_story_contents(
                    env,
                    &old_content,
                    &mut new_content,
                    owner_dialog_id,
                    &mut need_save_to_database,
                    &mut is_changed,
                );
                old_content = new_content.clone();
                story.content = Some(old_content);
            }
        }

        if is_changed || need_save_to_database {
            let story = self
                .stories
                .get(&story_full_id)
                .cloned()
                .expect("just inserted");
            self.change_story_files(env, story_full_id, &story, &old_file_ids);
        }

        if story_item.date <= 0 {
            tdutils::log_error!("Receive {story_full_id} sent at {}", story_item.date);
            story_item.date = 1;
        }
        if story_item.expire_date <= story_item.date {
            tdutils::log_error!(
                "Receive {story_full_id} sent at {}, but expired at {}",
                story_item.date,
                story_item.expire_date
            );
            story_item.expire_date = story_item.date + 1;
        }

        let is_live = content_type == StoryContentType::LiveStream;
        {
            let story = self.stories.get_mut(&story_full_id).expect("just inserted");
            if story.is_edited != story_item.edited
                || story.is_pinned != story_item.pinned
                || story.is_public != story_item.public
                || story.is_for_close_friends != story_item.close_friends
                || story.is_for_contacts != story_item.contacts
                || story.is_for_selected_contacts != story_item.selected_contacts
                || story.noforwards != story_item.noforwards
                || story.date != story_item.date
                || story.expire_date != story_item.expire_date
                || story.is_live != is_live
            {
                story.is_edited = story_item.edited;
                story.is_pinned = story_item.pinned;
                story.is_public = story_item.public;
                story.is_for_close_friends = story_item.close_friends;
                story.is_for_contacts = story_item.contacts;
                story.is_for_selected_contacts = story_item.selected_contacts;
                story.noforwards = story_item.noforwards;
                story.date = story_item.date;
                story.expire_date = story_item.expire_date;
                story.is_live = is_live;
                is_changed = true;
            }
        }
        if owner_dialog_id.get_type() == DialogType::User && !self.is_my_story(env, owner_dialog_id)
        {
            story_item.min = false;
        }
        let forward_info = story_item
            .fwd_from
            .take()
            .map(|fwd_header| StoryForwardInfo::from_fwd_header(env, fwd_header));
        {
            let story = self.stories.get_mut(&story_full_id).expect("just inserted");
            if story.forward_info != forward_info {
                story.forward_info = forward_info;
                is_changed = true;
            }
        }
        let sender_dialog_id = story_item
            .from_id
            .as_ref()
            .map(|peer| crate::story_ids::dialog_id_from_peer(Some(peer)))
            .unwrap_or_default();
        {
            let story = self.stories.get_mut(&story_full_id).expect("just inserted");
            if sender_dialog_id != story.sender_dialog_id {
                story.sender_dialog_id = sender_dialog_id;
                is_changed = true;
            }
        }
        let my_story = self.is_my_story(env, owner_dialog_id);
        if !story_item.min {
            let privacy_rules = UserPrivacySettingRules { rules: Vec::new() };
            let interaction_info =
                StoryInteractionInfo::from_story_views(env, story_item.views.take());
            let chosen_reaction_type =
                ReactionType::from_telegram(story_item.sent_reaction.as_ref());
            // the telegram privacy rules intake is [blocked: the privacy
            // track]; stories carry the rules only when the server sends
            // them, and the interim port keeps the previous rules.
            let _ = privacy_rules;

            let pending_reaction = self.being_set_story_reactions.get(&story_full_id).copied();
            let story = self.stories.get_mut(&story_full_id).expect("just inserted");
            if story.interaction_info != interaction_info
                || story.chosen_reaction_type != chosen_reaction_type
            {
                if let Some(mut pending) = pending_reaction {
                    tdutils::log_info!(
                        "Postpone {story_full_id} interaction info update, because there is a pending reaction"
                    );
                    pending |= 1;
                    self.being_set_story_reactions
                        .insert(story_full_id, pending);
                } else {
                    story.interaction_info = interaction_info;
                    story.chosen_reaction_type = chosen_reaction_type;
                    is_changed = true;
                }
            }

            if my_story {
                story_item.out = true;
            }
            if story.is_outgoing != story_item.out {
                story.is_outgoing = story_item.out;
                need_save_to_database = true;
            }

            let mut album_ids = StoryAlbumId::get_story_album_ids(&story_item.albums);
            album_ids.retain(|album_id| {
                if !album_id.is_valid() {
                    tdutils::log_error!("Receive {album_id}");
                    return false;
                }
                true
            });
            album_ids.sort();
            if story.album_ids != album_ids {
                story.album_ids = album_ids;
                is_changed = true;
            }
        }
        {
            let mut caption = caption;
            // the entities intake rides the FormattedText plane; the C++
            // `get_message_text` validation lands with the message-text
            // integration
            let story = self.stories.get_mut(&story_full_id).expect("just inserted");
            if story.caption != caption {
                story.caption = std::mem::take(&mut caption);
                let edited_edits_caption =
                    edited_state.is_some_and(|(_, _, edit_caption)| edit_caption);
                if edited_edits_caption {
                    need_save_to_database = true;
                } else {
                    is_changed = true;
                }
            }
        }
        {
            let mut media_areas: Vec<MediaArea> = Vec::new();
            for media_area in story_item.media_areas.drain(..) {
                if let Some(media_area) = media_area_from_telegram(media_area)
                    && media_area.is_valid()
                {
                    media_areas.push(media_area);
                }
            }
            let edited_edits_areas = edited_state.is_some_and(|(_, edit_areas, _)| edit_areas);
            let story = self.stories.get_mut(&story_full_id).expect("just inserted");
            if story.areas != media_areas {
                story.areas = media_areas;
                if edited_edits_areas {
                    need_save_to_database = true;
                } else {
                    is_changed = true;
                }
            }
        }

        {
            let story = self
                .stories
                .get(&story_full_id)
                .cloned()
                .expect("just inserted");
            self.add_story_dependencies(env, &story);
        }

        {
            let story = self
                .stories
                .get(&story_full_id)
                .expect("just inserted")
                .clone();
            self.on_story_changed(
                env,
                story_full_id,
                &story,
                is_changed,
                need_save_to_database,
                false,
            );
        }

        tdutils::log_info!("Receive {story_full_id}");

        if !env.is_bot()
            && self.is_active_story(
                env,
                self.stories.get(&story_full_id).expect("just inserted"),
            )
        {
            let has_active = self.active_stories.contains_key(&owner_dialog_id);
            if !has_active {
                if self.is_subscribed_to_dialog_stories(env, owner_dialog_id) {
                    self.load_dialog_expiring_stories(env, owner_dialog_id, "on_get_new_story");
                    if self.updated_active_stories.contains(&owner_dialog_id) {
                        self.on_update_active_stories(
                            env,
                            owner_dialog_id,
                            StoryId::default(),
                            vec![story_id],
                            Promise::default(),
                            "on_get_new_story 1",
                            false,
                        );
                    } else if old_story_id.is_valid() {
                        self.send_update_chat_active_stories(
                            env,
                            owner_dialog_id,
                            None,
                            "on_get_new_story 2",
                        );
                    }
                } else if old_story_id.is_valid() {
                    self.send_update_chat_active_stories(
                        env,
                        owner_dialog_id,
                        None,
                        "on_get_new_story 3",
                    );
                }
            } else {
                let active = self
                    .active_stories
                    .get(&owner_dialog_id)
                    .expect("checked")
                    .clone();
                if !active.story_ids.contains(&story_id) {
                    let mut story_ids = active.story_ids.clone();
                    story_ids.push(story_id);
                    let mut i = story_ids.len() - 1;
                    while i > 0 && story_ids[i - 1].get() > story_id.get() {
                        story_ids[i] = story_ids[i - 1];
                        i -= 1;
                    }
                    story_ids[i] = story_id;
                    self.on_update_active_stories(
                        env,
                        owner_dialog_id,
                        active.max_read_story_id,
                        story_ids,
                        Promise::default(),
                        "on_get_new_story",
                        false,
                    );
                } else if old_story_id.is_valid() {
                    self.send_update_chat_active_stories(
                        env,
                        owner_dialog_id,
                        Some(&active),
                        "on_get_new_story 4",
                    );
                }
            }
        }

        if old_story_id.is_valid() {
            let story_object = self.get_story_object(env, story_full_id);
            env.send_update(td_api::Update::updateStoryPostSucceeded(
                td_api::updateStoryPostSucceeded {
                    story: story_object,
                    old_story_id: old_story_id.get(),
                },
            ));
            if env.is_bot() {
                self.on_delete_story(env, story_full_id);
                return StoryId::default();
            }
        }

        story_id
    }

    /// `StoryManager::on_get_stories` (StoryManager.cpp:4841-4889).
    pub fn on_get_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        expected_story_ids: Vec<StoryId>,
        #[allow(clippy::boxed_local)] // the codegen boxes the stories umbrella
        stories: Box<telegram_api::stories_Stories>,
    ) -> (i32, Vec<StoryId>) {
        env.on_get_users(stories.users, "on_get_stories");
        env.on_get_chats(stories.chats, "on_get_stories");

        let mut story_ids = Vec::new();
        for story in stories.stories {
            match *story {
                telegram_api::StoryItem::storyItemDeleted(story_item) => {
                    self.on_get_deleted_story(env, owner_dialog_id, StoryId::new(story_item.id));
                }
                telegram_api::StoryItem::storyItemSkipped(story_item) => {
                    tdutils::log_error!("Receive storyItemSkipped in on_get_stories");
                    self.on_get_skipped_story(env, owner_dialog_id, story_item);
                }
                telegram_api::StoryItem::storyItem(story_item) => {
                    let story_id =
                        self.on_get_new_story(env, owner_dialog_id, Box::new(story_item));
                    if story_id.is_valid() {
                        story_ids.push(story_id);
                    }
                }
            }
        }

        let mut total_count = stories.count;
        if total_count < story_ids.len() as i32 {
            tdutils::log_error!(
                "Expected at most {total_count} stories, but receive {}",
                story_ids.len()
            );
            total_count = story_ids.len() as i32;
        }
        if !expected_story_ids.is_empty() {
            let mut all_story_ids: std::collections::HashSet<i32> = expected_story_ids
                .iter()
                .map(|story_id| story_id.get())
                .collect();
            for story_id in &story_ids {
                if !all_story_ids.remove(&story_id.get()) {
                    tdutils::log_error!("Receive {story_id}, but didn't request it");
                }
            }
            for story_id in all_story_ids {
                self.on_delete_story(
                    env,
                    StoryFullId::new(owner_dialog_id, StoryId::new(story_id)),
                );
            }
        }
        (total_count, story_ids)
    }

    /// `StoryManager::on_get_dialog_stories` (StoryManager.cpp:4891-4945).
    pub fn on_get_dialog_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        peer_stories: Option<telegram_api::PeerStories>,
        promise: Promise<()>,
    ) -> DialogId {
        let Some(peer_stories) = peer_stories else {
            if owner_dialog_id.is_valid() {
                tdutils::log_info!("Receive no stories in dialog {}", owner_dialog_id.get());
                self.on_update_active_stories(
                    env,
                    owner_dialog_id,
                    StoryId::default(),
                    Vec::new(),
                    promise,
                    "on_get_dialog_stories",
                    false,
                );
            } else {
                let mut promise = promise;
                promise.set_value(());
            }
            return owner_dialog_id;
        };

        let story_dialog_id = crate::story_ids::dialog_id_from_peer(peer_stories.peer.as_ref());
        if owner_dialog_id.is_valid() && owner_dialog_id != story_dialog_id {
            tdutils::log_error!("Receive stories from another dialog");
            self.on_update_active_stories(
                env,
                owner_dialog_id,
                StoryId::default(),
                Vec::new(),
                promise,
                "on_get_dialog_stories 2",
                false,
            );
            return owner_dialog_id;
        }
        if !story_dialog_id.is_valid() {
            tdutils::log_error!("Receive stories in dialog {}", story_dialog_id.get());
            let mut promise = promise;
            promise.set_value(());
            return owner_dialog_id;
        }
        let owner_dialog_id = story_dialog_id;

        let mut max_read_story_id = StoryId::new(peer_stories.max_read_id);
        if !max_read_story_id.is_server() && max_read_story_id != StoryId::default() {
            tdutils::log_error!("Receive max read {max_read_story_id}");
            max_read_story_id = StoryId::default();
        }

        let mut story_ids = Vec::new();
        for story in peer_stories.stories {
            match *story {
                telegram_api::StoryItem::storyItemDeleted(story_item) => {
                    self.on_get_deleted_story(env, owner_dialog_id, StoryId::new(story_item.id));
                }
                telegram_api::StoryItem::storyItemSkipped(story_item) => {
                    story_ids.push(self.on_get_skipped_story(env, owner_dialog_id, story_item));
                }
                telegram_api::StoryItem::storyItem(story_item) => {
                    story_ids.push(self.on_get_new_story(
                        env,
                        owner_dialog_id,
                        Box::new(story_item),
                    ));
                }
            }
        }

        self.on_update_active_stories(
            env,
            story_dialog_id,
            max_read_story_id,
            story_ids,
            promise,
            "on_get_dialog_stories 3",
            false,
        );
        story_dialog_id
    }

    /// `StoryManager::on_update_dialog_max_story_ids`
    /// (StoryManager.cpp:4947-4967) — the user/channel story-id surfaces
    /// are [blocked: user/channel stories].
    fn on_update_dialog_max_story_ids(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        max_read_story_id: StoryId,
    ) {
        let _ = env;
        match owner_dialog_id.get_type() {
            DialogType::User => env
                .on_update_user_max_read_story_id(owner_dialog_id.get_user_id(), max_read_story_id),
            DialogType::Channel => env.on_update_channel_max_read_story_id(
                owner_dialog_id.get_channel_id().get(),
                max_read_story_id,
            ),
            _ => {}
        }
    }

    /// `StoryManager::on_update_dialog_has_pinned_stories`
    /// (StoryManager.cpp:4985-4999).
    fn on_update_dialog_has_pinned_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        has_pinned_stories: bool,
    ) {
        match owner_dialog_id.get_type() {
            DialogType::User => env.on_update_user_has_pinned_stories(
                owner_dialog_id.get_user_id(),
                has_pinned_stories,
            ),
            DialogType::Channel => env.on_update_channel_has_pinned_stories(
                owner_dialog_id.get_channel_id().get(),
                has_pinned_stories,
            ),
            _ => {}
        }
    }

    /// `StoryManager::on_update_dialog_stories_hidden`
    /// (StoryManager.cpp:5001-5015).
    pub fn on_update_dialog_stories_hidden(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        stories_hidden: bool,
    ) {
        match owner_dialog_id.get_type() {
            DialogType::User => {
                env.on_update_user_stories_hidden(owner_dialog_id.get_user_id(), stories_hidden)
            }
            DialogType::Channel => env.on_update_channel_stories_hidden(
                owner_dialog_id.get_channel_id().get(),
                stories_hidden,
            ),
            _ => {}
        }
    }

    /// `StoryManager::update_active_stories` (StoryManager.cpp:5017-5024).
    /// [blocked: Td update sends].
    #[allow(dead_code)]
    fn update_active_stories(&mut self, env: &mut impl StoryManagerEnv, owner_dialog_id: DialogId) {
        if let Some(active_stories) = self.active_stories.get(&owner_dialog_id) {
            let story_ids = active_stories.story_ids.clone();
            let max_read_story_id = active_stories.max_read_story_id;
            self.on_update_active_stories(
                env,
                owner_dialog_id,
                max_read_story_id,
                story_ids,
                Promise::default(),
                "update_active_stories",
                false,
            );
        }
    }

    /// `StoryManager::on_update_active_stories`
    /// (StoryManager.cpp:5026-5122).
    /// The C++-faithful argument list (the convention's
    /// too_many_arguments note).
    #[allow(clippy::too_many_arguments)]
    pub fn on_update_active_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        mut max_read_story_id: StoryId,
        mut story_ids: Vec<StoryId>,
        promise: Promise<()>,
        source: &str,
        from_database: bool,
    ) {
        let mut from_database = from_database;
        let unix_time = env.unix_time();
        story_ids.retain(|story_id| {
            if !story_id.is_server() {
                return false;
            }
            let is_active = self
                .get_story(StoryFullId::new(owner_dialog_id, *story_id))
                .is_some_and(|story| unix_time < story.expire_date);
            if !is_active {
                tdutils::log_info!("Receive expired {story_id} from {source}");
            }
            is_active
        });
        from_database &= story_ids.len() == story_ids.capacity() || !story_ids.is_empty();
        let _ = from_database;
        // the C++ `remove_if` invalidation resets `from_database` on any
        // drop; the retain above reports it by comparison
        if story_ids.is_empty() || max_read_story_id.get() < story_ids[0].get() {
            max_read_story_id = StoryId::default();
        } else if max_read_story_id != StoryId::default() {
            assert!(max_read_story_id.is_server());
        }

        tdutils::log_info!(
            "Update active stories in dialog {} with max read {max_read_story_id} from {source}",
            owner_dialog_id.get()
        );

        if story_ids.is_empty() {
            self.on_update_dialog_max_story_ids(env, owner_dialog_id, StoryId::default());
            if let Some(active_stories) = self.active_stories.get(&owner_dialog_id) {
                tdutils::log_info!("Delete active stories for dialog {}", owner_dialog_id.get());
                let active = active_stories.clone();
                if active.story_list_id.is_valid() {
                    self.delete_active_stories_from_story_list(owner_dialog_id, &active);
                    let story_list = self.get_story_list_mut(active.story_list_id);
                    if !from_database
                        && story_list.is_reloaded_server_total_count
                        && story_list.server_total_count > story_list.ordered_stories.len() as i32
                    {
                        story_list.server_total_count -= 1;
                        let (state, server_total_count, server_has_more) = (
                            story_list.state.clone(),
                            story_list.server_total_count,
                            story_list.server_has_more,
                        );
                        self.save_story_list(
                            env,
                            active.story_list_id,
                            state,
                            server_total_count,
                            server_has_more,
                        );
                    }
                    self.update_story_list_sent_total_count(
                        env,
                        active.story_list_id,
                        "on_update_active_stories",
                    );
                }
                self.active_stories.remove(&owner_dialog_id);
                self.send_update_chat_active_stories(
                    env,
                    owner_dialog_id,
                    None,
                    "on_update_active_stories 1",
                );
            } else {
                self.max_read_story_ids.remove(&owner_dialog_id);
            }
            if !from_database {
                self.save_active_stories(env, owner_dialog_id, None, promise, source);
            } else {
                let mut promise = promise;
                promise.set_value(());
            }
            self.failed_to_load_active_stories.insert(owner_dialog_id);
            return;
        }
        self.failed_to_load_active_stories.remove(&owner_dialog_id);

        if !self.active_stories.contains_key(&owner_dialog_id) {
            tdutils::log_info!("Create active stories from {source}");
            let mut active_stories = ActiveStories::default();
            if let Some(old_max_read_story_id) = self.max_read_story_ids.remove(&owner_dialog_id)
                && old_max_read_story_id.get() > max_read_story_id.get()
                && old_max_read_story_id.get() >= story_ids[0].get()
            {
                max_read_story_id = old_max_read_story_id;
            }
            self.active_stories.insert(owner_dialog_id, active_stories);
            active_stories = ActiveStories::default();
            let _ = active_stories;
        }

        for story_id in &story_ids {
            if self.is_story_live(StoryFullId::new(owner_dialog_id, *story_id)) {
                self.on_update_dialog_max_story_ids(env, owner_dialog_id, StoryId::default());
                break;
            }
        }
        self.on_update_dialog_max_story_ids(env, owner_dialog_id, max_read_story_id);

        let mut need_save_to_database = false;
        {
            let active_stories = self
                .active_stories
                .get_mut(&owner_dialog_id)
                .expect("just checked");
            if active_stories.max_read_story_id != max_read_story_id
                || active_stories.story_ids != story_ids
            {
                need_save_to_database = true;
                active_stories.max_read_story_id = max_read_story_id;
                active_stories.story_ids = story_ids;
                self.update_active_stories_order(env, owner_dialog_id, &mut need_save_to_database);
                let active_stories = self
                    .active_stories
                    .get(&owner_dialog_id)
                    .expect("just updated")
                    .clone();
                self.send_update_chat_active_stories(
                    env,
                    owner_dialog_id,
                    Some(&active_stories),
                    "on_update_active_stories 2",
                );
            } else if self.update_active_stories_order(
                env,
                owner_dialog_id,
                &mut need_save_to_database,
            ) {
                let active_stories = self
                    .active_stories
                    .get(&owner_dialog_id)
                    .expect("just updated")
                    .clone();
                self.send_update_chat_active_stories(
                    env,
                    owner_dialog_id,
                    Some(&active_stories),
                    "on_update_active_stories 3",
                );
            }
        }

        if need_save_to_database && !from_database {
            let active_stories = self
                .active_stories
                .get(&owner_dialog_id)
                .expect("just updated");
            self.save_active_stories(env, owner_dialog_id, Some(active_stories), promise, source);
        } else {
            let mut promise = promise;
            promise.set_value(());
        }
    }

    /// `StoryManager::update_active_stories_order`
    /// (StoryManager.cpp:5124-5214).
    fn update_active_stories_order(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        need_save_to_database: &mut bool,
    ) -> bool {
        if env.is_bot() {
            return false;
        }

        let last_story_id = {
            let active_stories = self
                .active_stories
                .get(&owner_dialog_id)
                .expect("checked above");
            assert!(!active_stories.story_ids.is_empty());
            *active_stories.story_ids.last().expect("non-empty")
        };
        let last_story_date = self
            .get_story(StoryFullId::new(owner_dialog_id, last_story_id))
            .expect("checked above")
            .date;

        let mut new_private_order: i64 = 0;
        new_private_order += last_story_date as i64;
        if owner_dialog_id.get_type() == DialogType::User
            && env.is_user_premium(owner_dialog_id.get_user_id())
        {
            new_private_order += 1i64 << 33;
        }
        if owner_dialog_id == self.get_changelog_story_dialog_id(env) {
            new_private_order += 1i64 << 34;
        }
        let max_read_story_id = self
            .active_stories
            .get(&owner_dialog_id)
            .expect("checked above")
            .max_read_story_id;
        if max_read_story_id.get() < last_story_id.get() {
            new_private_order += 1i64 << 35;
        }
        {
            let active_stories = self
                .active_stories
                .get(&owner_dialog_id)
                .expect("checked above");
            if active_stories
                .story_ids
                .iter()
                .any(|story_id| self.is_story_live(StoryFullId::new(owner_dialog_id, *story_id)))
            {
                new_private_order += 1i64 << 36;
            }
        }
        if owner_dialog_id == env.get_my_dialog_id() {
            new_private_order += 1i64 << 37;
        }
        assert!(new_private_order != 0);

        let story_list_id = self.get_dialog_story_list_id(env, owner_dialog_id);
        let can_be_archived = story_list_id == StoryListId::Main
            && (owner_dialog_id.get_type() != DialogType::User
                || env.is_user_contact(owner_dialog_id.get_user_id()));
        let mut new_public_order: i64 = 0;
        if story_list_id.is_valid() {
            let list_last_story_date = self.get_story_list(story_list_id).list_last_story_date;
            if DialogDate::new(new_private_order, owner_dialog_id) <= list_last_story_date {
                new_public_order = new_private_order;
            }

            let (old_private_order, old_story_list_id) = {
                let active_stories = self
                    .active_stories
                    .get(&owner_dialog_id)
                    .expect("checked above");
                (active_stories.private_order, active_stories.story_list_id)
            };
            if old_private_order != new_private_order || old_story_list_id != story_list_id {
                let active_stories = self
                    .active_stories
                    .get(&owner_dialog_id)
                    .expect("checked above")
                    .clone();
                self.delete_active_stories_from_story_list(owner_dialog_id, &active_stories);
                let is_inserted = self
                    .get_story_list_mut(story_list_id)
                    .ordered_stories
                    .insert(DialogDate::new(new_private_order, owner_dialog_id));
                assert!(is_inserted);

                if old_story_list_id != story_list_id && old_story_list_id.is_valid() {
                    self.update_story_list_sent_total_count(
                        env,
                        old_story_list_id,
                        "update_active_stories_order 1",
                    );
                }
                self.update_story_list_sent_total_count(
                    env,
                    story_list_id,
                    "update_active_stories_order 2",
                );
            }
        } else {
            let (old_private_order, old_story_list_id) = {
                let active_stories = self
                    .active_stories
                    .get(&owner_dialog_id)
                    .expect("checked above");
                (active_stories.private_order, active_stories.story_list_id)
            };
            let _ = old_private_order;
            if old_story_list_id.is_valid() {
                let active_stories = self
                    .active_stories
                    .get(&owner_dialog_id)
                    .expect("checked above")
                    .clone();
                self.delete_active_stories_from_story_list(owner_dialog_id, &active_stories);
                self.update_story_list_sent_total_count(
                    env,
                    old_story_list_id,
                    "update_active_stories_order 3",
                );
            }
        }

        let (old_private_order, old_public_order, old_story_list_id, old_can_be_archived) = {
            let active_stories = self
                .active_stories
                .get(&owner_dialog_id)
                .expect("checked above");
            (
                active_stories.private_order,
                active_stories.public_order,
                active_stories.story_list_id,
                active_stories.can_be_archived,
            )
        };
        if old_private_order != new_private_order
            || old_public_order != new_public_order
            || old_story_list_id != story_list_id
            || old_can_be_archived != can_be_archived
        {
            tdutils::log_info!(
                "Update order of active stories to {new_private_order}/{new_public_order}"
            );
            if old_private_order != new_private_order || old_story_list_id != story_list_id {
                *need_save_to_database = true;
            }
            let mut needs_order_update = false;
            {
                let active_stories = self
                    .active_stories
                    .get_mut(&owner_dialog_id)
                    .expect("checked above");
                active_stories.private_order = new_private_order;
                if old_public_order != new_public_order
                    || old_story_list_id != story_list_id
                    || old_can_be_archived != can_be_archived
                {
                    if old_story_list_id != story_list_id {
                        if old_story_list_id.is_valid() && old_public_order != 0 {
                            active_stories.public_order = 0;
                            needs_order_update = true;
                        }
                        active_stories.story_list_id = story_list_id;
                    }
                    active_stories.public_order = new_public_order;
                    active_stories.can_be_archived = can_be_archived;
                }
            }
            if needs_order_update {
                let updated = self.active_stories.get(&owner_dialog_id).cloned();
                self.send_update_chat_active_stories(
                    env,
                    owner_dialog_id,
                    updated.as_ref(),
                    "update_active_stories_order",
                );
            }
            if needs_order_update {
                return true;
            }
        }

        false
    }

    /// `StoryManager::delete_active_stories_from_story_list`
    /// (StoryManager.cpp:5216-5224).
    fn delete_active_stories_from_story_list(
        &mut self,
        owner_dialog_id: DialogId,
        active_stories: &ActiveStories,
    ) {
        if !active_stories.story_list_id.is_valid() {
            return;
        }
        let removed = self
            .get_story_list_mut(active_stories.story_list_id)
            .ordered_stories
            .remove(&DialogDate::new(
                active_stories.private_order,
                owner_dialog_id,
            ));
        assert!(removed);
    }

    /// `StoryManager::on_update_story_id` (StoryManager.cpp:5293-5322).
    pub fn on_update_story_id(
        &mut self,
        env: &mut impl StoryManagerEnv,
        random_id: i64,
        new_story_id: StoryId,
        source: &str,
    ) {
        if !new_story_id.is_server() {
            tdutils::log_error!("Receive {new_story_id} with random_id {random_id} from {source}");
            return;
        }

        let Some(old_story_full_id) = self.being_sent_stories.remove(&random_id) else {
            // update about a new story sent from another device
            tdutils::log_info!(
                "Receive not sent outgoing {new_story_id} with random_id = {random_id}"
            );
            return;
        };
        if self
            .being_sent_story_random_ids
            .remove(&old_story_full_id)
            .is_none()
        {
            tdutils::log_error!("Have inconsistent being-sent stories for random_id {random_id}");
        }

        if !self.have_story_force(env, old_story_full_id) {
            tdutils::log_info!("Can't find sent {old_story_full_id}");
            return;
        }

        let old_story_id = old_story_full_id.get_story_id();
        let new_story_full_id = StoryFullId::new(old_story_full_id.get_dialog_id(), new_story_id);

        tdutils::log_info!("Save correspondence from {new_story_full_id} to {old_story_id}");
        assert!(!old_story_id.is_server());
        self.update_story_ids
            .insert(new_story_full_id, old_story_id);
    }

    /// `StoryManager::on_update_read_stories` (StoryManager.cpp:5324-5356).
    pub fn on_update_read_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        max_read_story_id: StoryId,
    ) -> bool {
        if !env.have_dialog_force(owner_dialog_id, "on_update_read_stories") {
            tdutils::log_info!(
                "Can't read stories in unknown dialog {}",
                owner_dialog_id.get()
            );
            return false;
        }
        if max_read_story_id != StoryId::default() && !max_read_story_id.is_server() {
            tdutils::log_error!("Receive max read {max_read_story_id}");
            return false;
        }
        let loaded = self
            .get_active_stories_force(env, owner_dialog_id, "on_update_read_stories")
            .is_some();
        if !loaded {
            tdutils::log_info!(
                "Can't find active stories in dialog {}",
                owner_dialog_id.get()
            );
            let old_max_read_story_id = self
                .max_read_story_ids
                .get(&owner_dialog_id)
                .copied()
                .unwrap_or_default();
            if max_read_story_id.get() > old_max_read_story_id.get() {
                tdutils::log_info!(
                    "Set max read story identifier in dialog {}",
                    owner_dialog_id.get()
                );
                self.max_read_story_ids
                    .insert(owner_dialog_id, max_read_story_id);
                self.on_update_dialog_max_story_ids(env, owner_dialog_id, max_read_story_id);
                return true;
            }
        } else {
            let active_max_read_story_id = self
                .active_stories
                .get(&owner_dialog_id)
                .expect("checked")
                .max_read_story_id;
            if max_read_story_id.get() > active_max_read_story_id.get() {
                tdutils::log_info!(
                    "Update max read story identifier in dialog {}",
                    owner_dialog_id.get()
                );
                let story_ids = self
                    .active_stories
                    .get(&owner_dialog_id)
                    .expect("checked")
                    .story_ids
                    .clone();
                self.on_update_active_stories(
                    env,
                    owner_dialog_id,
                    max_read_story_id,
                    story_ids,
                    Promise::default(),
                    "on_update_read_stories",
                    false,
                );
                return true;
            }
        }
        false
    }

    /// `StoryManager::save_read_stories_on_server_log_event` — [blocked:
    /// binlog]; the log event id is the zero stub.
    fn save_read_stories_on_server_log_event(
        &mut self,
        env: &mut impl StoryManagerEnv,
        dialog_id: DialogId,
        max_story_id: StoryId,
    ) -> u64 {
        let _ = (env, dialog_id, max_story_id);
        0
    }

    /// `StoryManager::read_stories_on_server` (StoryManager.cpp:3614-3621).
    fn read_stories_on_server(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_id: StoryId,
        mut log_event_id: u64,
    ) {
        assert!(story_id.is_server());
        if log_event_id == 0 && env.use_message_database() {
            log_event_id =
                self.save_read_stories_on_server_log_event(env, owner_dialog_id, story_id);
        }
        let _ = log_event_id;
        // `ReadStoriesQuery::send` (StoryManager.cpp:232-240) — the Read
        // peer with the max id; the null band is the C++ on_error head
        // (the promise is the binlog-blocked erase event, a no-op here).
        match env.get_input_peer(owner_dialog_id, crate::user_manager::AccessRights::Read) {
            Some(input_peer) => self.send_story_net_query(
                StoryNetKind::ReadStories {
                    dialog_id: owner_dialog_id,
                },
                telegram_api::Function::stories_readStories(telegram_api::stories_readStories {
                    peer: Some(Box::new(input_peer)),
                    max_id: story_id.get(),
                }),
                "stories.readStories",
            ),
            None => {
                let status = Status::error(400, "Can't access the chat");
                env.on_get_dialog_error(owner_dialog_id, &status, "ReadStoriesQuery");
            }
        }
    }

    /// `StoryManager::has_unexpired_viewers` (StoryManager.cpp:3643-3647).
    fn has_unexpired_viewers(
        &self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
        story: &Story,
    ) -> bool {
        self.is_my_story(env, story_full_id.get_dialog_id())
            && story_full_id.get_story_id().is_server()
            && !story.is_live
            && env.unix_time() < self.get_story_viewers_expire_date_ref(env, story)
    }

    /// `StoryManager::can_get_story_viewers` (StoryManager.cpp:3623-3641).
    pub fn can_get_story_viewers(
        &self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
        story: &Story,
        unix_time: i32,
    ) -> Status {
        if story.is_live {
            return Status::error(400, "Story viewers aren't available");
        }
        if !self.is_my_story(env, story_full_id.get_dialog_id()) {
            return Status::error(400, "Story must be outgoing");
        }
        if !story_full_id.get_story_id().is_server() {
            return Status::error(400, "Story is not sent yet");
        }
        if story.interaction_info.get_reaction_count() > 0 {
            return Status::ok();
        }
        if story.interaction_info.has_hidden_viewers()
            && unix_time >= self.get_story_viewers_expire_date_ref(env, story)
        {
            return Status::error(400, "Story is too old");
        }
        Status::ok()
    }

    /// `StoryManager::has_suggested_reaction` (StoryManager.cpp:3406-3412).
    fn has_suggested_reaction(story: &Story, reaction_type: &ReactionType) -> bool {
        if reaction_type.is_empty() || reaction_type.is_paid_reaction() {
            return false;
        }
        story
            .areas
            .iter()
            .any(|area| area.has_reaction_type(reaction_type))
    }

    /// `StoryManager::can_use_story_reaction` (StoryManager.cpp:3414-3434).
    fn can_use_story_reaction(
        &self,
        env: &mut impl StoryManagerEnv,
        story: &Story,
        reaction_type: &ReactionType,
    ) -> bool {
        if story.is_live {
            return false;
        }
        if reaction_type.is_empty() {
            return true;
        }
        if reaction_type.is_custom_reaction() {
            if env.get_option_boolean("is_premium") {
                return true;
            }
            if Self::has_suggested_reaction(story, reaction_type) {
                return true;
            }
            return false;
        }
        if reaction_type.is_paid_reaction() {
            return false;
        }
        env.is_active_reaction(reaction_type)
    }

    /// `StoryManager::on_story_chosen_reaction_changed`
    /// (StoryManager.cpp:3436-3452).
    fn on_story_chosen_reaction_changed(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
        reaction_type: ReactionType,
    ) {
        let Some(mut story) = self.stories.get(&story_full_id).cloned() else {
            return;
        };
        if story.chosen_reaction_type == reaction_type {
            return;
        }

        if story_full_id.get_dialog_id().get_type() != DialogType::User {
            let need_add = Self::has_suggested_reaction(&story, &reaction_type);
            let need_remove = Self::has_suggested_reaction(&story, &story.chosen_reaction_type);
            if need_add || need_remove {
                let empty_reaction_type = ReactionType::default();
                story.interaction_info.set_chosen_reaction_type(
                    if need_add {
                        &reaction_type
                    } else {
                        &empty_reaction_type
                    },
                    &story.chosen_reaction_type,
                );
            }
        }
        story.chosen_reaction_type = reaction_type;
        self.stories.insert(story_full_id, story);
        let story = self
            .stories
            .get(&story_full_id)
            .expect("just inserted")
            .clone();
        self.on_story_changed(env, story_full_id, &story, true, true, false);
    }

    /// `StoryManager::set_story_reaction` (StoryManager.cpp:3454-3490) —
    /// the query send is the `[blocked: net]` seam; the promise parks.
    pub fn set_story_reaction(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
        reaction_type: ReactionType,
        add_to_recent: bool,
        mut promise: Promise<()>,
    ) {
        let owner_dialog_id = story_full_id.get_dialog_id();
        let mut status =
            env.check_dialog_access(owner_dialog_id, false, false, "set_story_reaction");
        if status.is_error() {
            promise.set_error(status.move_as_error());
            return;
        }
        let status = self.check_story_id(story_full_id.get_story_id(), true);
        if status.is_error() {
            promise.set_error(status);
            return;
        }

        let Some(story) = self
            .get_story_force(env, story_full_id, "set_story_reaction")
            .cloned()
        else {
            promise.set_error(Status::error(400, "Story not found"));
            return;
        };

        if !self.can_use_story_reaction(env, &story, &reaction_type) {
            promise.set_error(Status::error(
                400,
                "The reaction isn't available for the story",
            ));
            return;
        }

        if story.chosen_reaction_type == reaction_type {
            promise.set_value(());
            return;
        }

        if add_to_recent {
            env.add_recent_reaction(&reaction_type);
        }

        self.on_story_chosen_reaction_changed(env, story_full_id, reaction_type.clone());

        *self
            .being_set_story_reactions
            .entry(story_full_id)
            .or_default() += 2;

        // the C++ answers through `on_set_story_reaction` after the query
        // (the promise parks before the send leg — the handler owns it)
        self.pending_story_reactions
            .entry(story_full_id)
            .or_default()
            .push(promise);
        // `SendStoryReactionQuery::send` (StoryManager.cpp:265-281) — the
        // Read peer with the reaction (the empty reaction clears the
        // recent flag). The null band runs the C++ on_error head and the
        // promise lambda's error arm (`on_set_story_reaction` reloads).
        let mut add_to_recent = add_to_recent;
        if reaction_type.is_empty() {
            add_to_recent = false;
        }
        match env.get_input_peer(owner_dialog_id, crate::user_manager::AccessRights::Read) {
            Some(input_peer) => {
                let reaction = reaction_type.get_input_reaction();
                self.send_story_net_query(
                    StoryNetKind::SendStoryReaction { story_full_id },
                    telegram_api::Function::stories_sendReaction(
                        telegram_api::stories_sendReaction {
                            flags: 0,
                            add_to_recent,
                            peer: Some(Box::new(input_peer)),
                            story_id: story_full_id.get_story_id().get(),
                            reaction: Some(reaction),
                        },
                    ),
                    "stories.sendReaction",
                );
            }
            None => {
                let status = Status::error(400, "Can't access the chat");
                env.on_get_dialog_error(owner_dialog_id, &status, "SendStoryReactionQuery");
                self.on_set_story_reaction(env, story_full_id, Result::from_status(status.clone()));
            }
        }
    }

    /// `StoryManager::on_set_story_reaction` (StoryManager.cpp:3492-3516).
    pub fn on_set_story_reaction(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
        result: Result<()>,
    ) {
        let Some(promises) = self.pending_story_reactions.remove(&story_full_id) else {
            return;
        };
        let need_reload_story = result.is_error();

        let pending = self
            .being_set_story_reactions
            .entry(story_full_id)
            .or_default();
        *pending = pending.saturating_sub(2);
        let needs_reload = *pending == 1;
        if *pending <= 1 {
            self.being_set_story_reactions.remove(&story_full_id);
            if needs_reload {
                // the pending interaction-info update needs the reload
                for mut promise in promises {
                    promise.set_result(result.clone());
                }
                self.reload_story(
                    env,
                    story_full_id,
                    Promise::default(),
                    "on_set_story_reaction",
                );
                return;
            }
        }

        if !self.have_story_force(env, story_full_id) {
            for mut promise in promises {
                promise.set_value(());
            }
            return;
        }

        if need_reload_story {
            self.reload_story(
                env,
                story_full_id,
                Promise::default(),
                "on_set_story_reaction",
            );
        }

        for mut promise in promises {
            promise.set_result(result.clone());
        }
    }

    /// `StoryManager::on_increment_story_views`
    /// (StoryManager.cpp:3575-3588).
    pub fn on_increment_story_views(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
    ) {
        let Some(story_views) = self.pending_story_views.get_mut(&owner_dialog_id) else {
            return;
        };
        assert!(story_views.has_query);
        story_views.has_query = false;
        let has_more = !story_views.story_ids.is_empty();
        if !has_more {
            self.pending_story_views.remove(&owner_dialog_id);
            return;
        }
        increment_story_views_impl(self, env, owner_dialog_id);
    }

    /// `StoryManager::open_story` (StoryManager.cpp:3286-3341).
    pub fn open_story(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_id: StoryId,
        mut promise: Promise<()>,
    ) {
        let mut status = env.check_dialog_access(owner_dialog_id, false, false, "open_story");
        if status.is_error() {
            promise.set_error(status.move_as_error());
            return;
        }
        let status = self.check_story_id(story_id, false);
        if status.is_error() {
            promise.set_error(status);
            return;
        }

        let story_full_id = StoryFullId::new(owner_dialog_id, story_id);
        let Some(story) = self.get_story(story_full_id).cloned() else {
            promise.set_value(());
            return;
        };

        if self.can_get_story_view_count(env, owner_dialog_id)
            && story_id.is_server()
            && !story.is_live
        {
            let open_count = self
                .opened_stories_with_view_count
                .entry(story_full_id)
                .or_default();
            *open_count += 1;
            if *open_count == 1 {
                // the handler's `dialog_id_`/`story_ids_` members park for
                // the answer's `on_get_story_views` intake
                self.pending_get_stories_views
                    .push((owner_dialog_id, vec![story_id]));
                env.send_get_stories_views_query(owner_dialog_id, &[story_id]);
            }
        }

        if story.content.is_none() {
            promise.set_value(());
            return;
        }

        if story_id.is_server() {
            let open_count = self.opened_stories.entry(story_full_id).or_default();
            *open_count += 1;
            if *open_count == 1 {
                // `story_reload_timeout_` — [blocked: Timeout actor]; the
                // reload decision runs from the drain hook
            }
        }

        for file_id in self.get_story_file_ids(env, &story) {
            env.check_local_location_async(file_id);
        }

        let is_active = self.is_active_story(env, &story);
        let need_increment_story_views =
            story_id.is_server() && !is_active && story.is_pinned && !story.is_live;
        let need_read_story = story_id.is_server() && is_active;

        if need_increment_story_views {
            let story_views = self.pending_story_views.entry(owner_dialog_id).or_default();
            story_views.story_ids.insert(story_id.get());
            let has_query = story_views.has_query;
            if !has_query {
                increment_story_views_impl(self, env, owner_dialog_id);
            }
        }

        if need_read_story && self.on_update_read_stories(env, owner_dialog_id, story_id) {
            self.read_stories_on_server(env, owner_dialog_id, story_id, 0);
        }

        promise.set_value(());
    }

    /// `StoryManager::close_story` (StoryManager.cpp:3343-3377).
    pub fn close_story(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_id: StoryId,
        mut promise: Promise<()>,
    ) {
        let mut status = env.check_dialog_access(owner_dialog_id, false, false, "close_story");
        if status.is_error() {
            promise.set_error(status.move_as_error());
            return;
        }
        let status = self.check_story_id(story_id, false);
        if status.is_error() {
            promise.set_error(status);
            return;
        }

        let story_full_id = StoryFullId::new(owner_dialog_id, story_id);
        if self.can_get_story_view_count(env, owner_dialog_id)
            && story_id.is_server()
            && let Some(open_count) = self.opened_stories_with_view_count.get_mut(&story_full_id)
        {
            *open_count -= 1;
            if *open_count == 0 {
                self.opened_stories_with_view_count.remove(&story_full_id);
            }
        }

        if self.get_story(story_full_id).is_none() {
            promise.set_value(());
            return;
        }

        if story_id.is_server() {
            let open_count = self.opened_stories.entry(story_full_id).or_default();
            if *open_count > 0 {
                *open_count -= 1;
                if *open_count == 0 {
                    self.opened_stories.remove(&story_full_id);
                    // `story_reload_timeout_.cancel_timeout` — [blocked:
                    // Timeout actor]
                }
            }
        }

        promise.set_value(());
    }

    /// `StoryManager::view_story_message` (StoryManager.cpp:3379-3388).
    pub fn view_story_message(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
    ) {
        if !story_full_id.get_story_id().is_server() {
            return;
        }

        let need_reload = match self.get_story_force(env, story_full_id, "view_story_message") {
            Some(story) => story.receive_date < env.unix_time() - VIEWED_STORY_POLL_PERIOD,
            None => true,
        };
        if need_reload {
            self.reload_story(env, story_full_id, Promise::default(), "view_story_message");
        }
    }

    /// `StoryManager::on_story_replied` (StoryManager.cpp:3390-3404).
    pub fn on_story_replied(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
        replier_user_id: UserId,
    ) {
        if !replier_user_id.is_valid()
            || replier_user_id == env.get_my_id()
            || !story_full_id.get_story_id().is_server()
        {
            return;
        }
        let Some(story) = self
            .get_story_force(env, story_full_id, "on_story_replied")
            .cloned()
        else {
            return;
        };
        if !self.is_my_story(env, story_full_id.get_dialog_id()) || story.is_live {
            return;
        }

        if story.content.is_some()
            && env.unix_time() < self.get_story_viewers_expire_date_ref(env, &story)
            && story
                .interaction_info
                .definitely_has_no_user(replier_user_id)
        {
            env.send_get_stories_views_query(
                story_full_id.get_dialog_id(),
                &[story_full_id.get_story_id()],
            );
            self.pending_get_stories_views.push((
                story_full_id.get_dialog_id(),
                vec![story_full_id.get_story_id()],
            ));
        }
    }

    /// `StoryManager::on_get_story_views` (StoryManager.cpp:5521-5552).
    pub fn on_get_story_views(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_ids: &[StoryId],
        story_views: Option<telegram_api::stories_StoryViews>,
    ) {
        let Some(story_views) = story_views else {
            return;
        };
        env.on_get_users(story_views.users, "on_get_story_views");
        if story_ids.len() != story_views.views.len() {
            tdutils::log_error!("Receive invalid views");
            return;
        }
        for (index, story_id) in story_ids.iter().enumerate() {
            assert!(story_id.is_server());

            let story_full_id = StoryFullId::new(owner_dialog_id, *story_id);
            if self
                .stories
                .get(&story_full_id)
                .is_none_or(|story| story.content.is_none())
            {
                continue;
            }

            let interaction_info =
                StoryInteractionInfo::from_story_views(env, Some(story_views.views[index].clone()));
            let pending_reaction = self.being_set_story_reactions.get(&story_full_id).copied();
            let story = self.stories.get_mut(&story_full_id).expect("checked above");
            if story.interaction_info != interaction_info {
                if let Some(mut pending) = pending_reaction {
                    tdutils::log_info!(
                        "Postpone {story_full_id} interaction info update, because there is a pending reaction"
                    );
                    pending |= 1;
                    self.being_set_story_reactions
                        .insert(story_full_id, pending);
                } else {
                    story.interaction_info = interaction_info;
                    let story = self
                        .stories
                        .get(&story_full_id)
                        .expect("checked above")
                        .clone();
                    self.on_story_changed(env, story_full_id, &story, true, true, false);
                }
            }
        }
    }

    /// The `StoryManager::StoryManager` constructor block
    /// (StoryManager.cpp:1999-2030) + `start_up` (2040-2065) collapse: the
    /// first auth-true run loads the story-list states, the stealth-mode pmc
    /// row and kicks the archive synchronization and the expired-stories
    /// scan; idempotent.
    /// The C++ nested-guard style (the convention's collapsible-if
    /// note).
    #[allow(clippy::collapsible_if)]
    pub fn init(&mut self, env: &mut impl StoryManagerEnv) {
        // the constructor block (cpp:2011-2029)
        if env.use_message_database() && env.is_authorized() && !env.is_bot() {
            for story_list_id in [StoryListId::Main, StoryListId::Archive] {
                let value = match env.story_db() {
                    Some(db) => db.get_active_story_list_state(story_list_id),
                    None => continue,
                };
                if let Result::Ok(value) = value {
                    if !value.is_empty() {
                        match SavedStoryList::parse(&mut Parser::new(&value)) {
                            Err(_) => {
                                tdutils::log_error!(
                                    "Load invalid state for {story_list_id} from database"
                                );
                            }
                            Ok(saved_story_list) => {
                                tdutils::log_info!(
                                    "Load state for {story_list_id} from database: {}",
                                    saved_story_list.state
                                );
                                let story_list = self.get_story_list_mut(story_list_id);
                                story_list.state = saved_story_list.state;
                                story_list.server_total_count = saved_story_list.total_count.max(0);
                                story_list.server_has_more = saved_story_list.has_more;
                                story_list.database_has_more = true;
                            }
                        }
                    }
                }
            }
        }

        if !Self::is_active(env) {
            return;
        }
        if self.is_inited {
            return;
        }
        self.is_inited = true;

        // start_up (cpp:2040-2065)
        let stealth_mode_str =
            env.binlog_pmc_get_raw(Self::get_story_stealth_mode_key().as_bytes());
        if !stealth_mode_str.is_empty() {
            // `log_event_parse` framing — the version prefix is consumed
            // by the parser constructor.
            let parsed = Parser::new_log_event(&stealth_mode_str)
                .and_then(|mut parser| StoryStealthMode::parse(&mut parser));
            match parsed {
                Err(status) => {
                    tdutils::log_error!("Failed to parse stealth mode from binlog: {status}");
                }
                Ok(mut stealth_mode) => {
                    stealth_mode.update(env.unix_time());
                    tdutils::log_info!("{stealth_mode}");
                    self.stealth_mode = stealth_mode;
                    if stealth_mode.is_empty() {
                        env.binlog_pmc_erase(Self::get_story_stealth_mode_key().as_bytes());
                    }
                }
            }
        }
        self.send_update_story_stealth_mode(env);

        self.try_synchronize_archive_all_stories(env);
        self.load_expired_database_stories(env);

        for story_list_id in [StoryListId::Main, StoryListId::Archive] {
            self.update_story_list_sent_total_count(env, story_list_id, "start_up");
        }
    }

    /// `StoryManager::try_synchronize_archive_all_stories`
    /// (StoryManager.cpp:2935-2953).
    pub fn try_synchronize_archive_all_stories(&mut self, env: &mut impl StoryManagerEnv) {
        if self.has_active_synchronize_archive_all_stories_query {
            return;
        }
        if !env.get_option_boolean("need_synchronize_archive_all_stories") {
            return;
        }

        self.has_active_synchronize_archive_all_stories_query = true;
        let archive_all_stories = env.get_option_boolean("archive_all_stories");
        // `ToggleAllStoriesHiddenQuery::send` (StoryManager.cpp:169-172)
        // — the promise lambda captures the flag for
        // `on_synchronized_archive_all_stories` (it fires on both bands).
        self.send_story_net_query(
            StoryNetKind::ToggleAllStoriesHidden {
                set_archive_all_stories: archive_all_stories,
            },
            telegram_api::Function::stories_toggleAllStoriesHidden(
                telegram_api::stories_toggleAllStoriesHidden {
                    hidden: archive_all_stories,
                },
            ),
            "stories.toggleAllStoriesHidden",
        );
    }

    /// `StoryManager::on_synchronized_archive_all_stories`
    /// (StoryManager.cpp:2955-2971).
    pub fn on_synchronized_archive_all_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        set_archive_all_stories: bool,
        result: Result<()>,
    ) {
        self.has_active_synchronize_archive_all_stories_query = false;

        let archive_all_stories = env.get_option_boolean("archive_all_stories");
        if archive_all_stories != set_archive_all_stories {
            return self.try_synchronize_archive_all_stories(env);
        }
        env.set_option_empty("need_synchronize_archive_all_stories");

        if result.is_error() {
            // `ConfigManager::reload_app_config` — [blocked: M.3 config
            // reload seam]; the option empty above drives the retry.
            tdutils::log_info!(
                "reload_app_config after the failed archive-all sync — [blocked: ConfigManager]"
            );
        }
    }

    /// `StoryManager::reload_all_read_stories` (StoryManager.cpp:2931-2933).
    pub fn reload_all_read_stories(&mut self, env: &mut impl StoryManagerEnv) {
        let _ = env;
        // `GetAllReadPeerStoriesQuery::send` (StoryManager.cpp:142-144)
        // — promise-less; the answer rides the updates engine with an
        // empty promise.
        self.send_story_net_query(
            StoryNetKind::GetAllReadPeerStories,
            telegram_api::Function::stories_getAllReadPeerStories(
                telegram_api::stories_getAllReadPeerStories,
            ),
            "stories.getAllReadPeerStories",
        );
    }

    /// `StoryManager::load_expired_database_stories`
    /// (StoryManager.cpp:2188-2206) — the scan reads the expiring rows; the
    /// C++ drops non-still-alive stories in the answer half.
    pub fn load_expired_database_stories(&mut self, env: &mut impl StoryManagerEnv) {
        if !env.use_message_database() {
            return;
        }

        tdutils::log_info!(
            "Load {} expired stories",
            self.load_expired_database_stories_next_limit
        );
        let unix_time = env.unix_time();
        let next_limit = self.load_expired_database_stories_next_limit;
        let stories = match env.story_db() {
            Some(db) => db.get_expiring_stories(unix_time - 1, next_limit),
            None => Vec::new(),
        };
        self.on_load_expired_database_stories(env, stories);
    }

    /// `StoryManager::on_load_expired_database_stories`
    /// (StoryManager.cpp:2208-2232).
    pub fn on_load_expired_database_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        stories: Vec<StoryDbStory>,
    ) {
        let next_request_delay;
        if stories.len() == self.load_expired_database_stories_next_limit as usize {
            self.load_expired_database_stories_next_limit =
                (self.load_expired_database_stories_next_limit * 2).min(1 << 30);
            next_request_delay = 1;
        } else {
            self.load_expired_database_stories_next_limit = DEFAULT_LOADED_EXPIRED_STORIES;
            next_request_delay = 360;
        }
        let _ = next_request_delay; // the Timeout actor re-arm is [blocked]

        for database_story in stories {
            if self
                .parse_story(env, database_story.story_full_id, &database_story.data)
                .is_some()
            {
                tdutils::log_error!("Receive non-expired {}", database_story.story_full_id);
            }
        }
    }

    /// `StoryManager::reload_story` (StoryManager.cpp:5644-5672).
    pub fn reload_story(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
        promise: Promise<()>,
        source: &str,
    ) {
        if self.deleted_story_full_ids.contains(&story_full_id) {
            let mut promise = promise;
            promise.set_value(());
            return;
        }
        let last_reloaded_at = self
            .inaccessible_story_full_ids
            .get(&story_full_id)
            .copied()
            .unwrap_or(0.0);
        if last_reloaded_at > 0.0
            && last_reloaded_at >= env.unix_time() as f64 - OPENED_STORY_POLL_PERIOD as f64 / 2.0
        {
            let mut promise = promise;
            promise.set_value(());
            return;
        }

        tdutils::log_info!("Reload {story_full_id} from {source}");
        let story_id = story_full_id.get_story_id();
        let status = self.check_story_id(story_id, true);
        if status.is_error() {
            let mut promise = promise;
            promise.set_error(status);
            return;
        }

        let queries = self.reload_story_queries.entry(story_full_id).or_default();
        if !queries.is_empty() {
            queries.push(promise);
            return;
        }
        queries.push(promise);
        env.send_get_stories_by_id_query(story_full_id.get_dialog_id(), &[story_id]);
    }

    /// `StoryManager::on_reload_story` (StoryManager.cpp:5674-5689).
    pub fn on_reload_story(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
        result: Result<()>,
    ) {
        let Some(promises) = self.reload_story_queries.remove(&story_full_id) else {
            return;
        };
        // the C++ parks chained getStory promises on the same reload
        if let Some(get_promises) = self.pending_get_story.remove(&story_full_id) {
            if result.is_ok() {
                let story_object = self.get_story_object(env, story_full_id);
                for mut promise in get_promises {
                    promise.set_value(story_object.clone());
                }
            } else {
                for mut promise in get_promises {
                    promise.set_error(result.error().clone());
                }
            }
        }
        for mut promise in promises {
            promise.set_result(result.clone());
        }
    }

    /// `StoryManager::get_story` (StoryManager.cpp:5708-5731).
    /// The C++ nested-guard style (the convention's collapsible-if
    /// note).
    #[allow(clippy::collapsible_if)]
    pub fn get_story_request(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_id: StoryId,
        only_local: bool,
        mut promise: Promise<Option<td_api::Story>>,
    ) {
        let mut status = env.check_dialog_access(owner_dialog_id, false, false, "get_story");
        if status.is_error() {
            promise.set_error(status.move_as_error());
            return;
        }
        let status = self.check_story_id(story_id, false);
        if status.is_error() {
            promise.set_error(status);
            return;
        }

        let story_full_id = StoryFullId::new(owner_dialog_id, story_id);
        let story = self
            .get_story_force(env, story_full_id, "get_story")
            .cloned();
        if let Some(story) = story {
            if story.content.is_some() {
                if !story.is_update_sent {
                    self.send_update_story(env, story_full_id, &story);
                }
                promise.set_value(self.get_story_object(env, story_full_id));
                return;
            }
        }
        if only_local || !story_id.is_server() {
            promise.set_value(None);
            return;
        }

        self.pending_get_story
            .entry(story_full_id)
            .or_default()
            .push(promise);
        self.reload_story(env, story_full_id, Promise::default(), "get_story");
    }

    /// `StoryManager::do_get_story` (StoryManager.cpp:5733-5744) — folded
    /// into [`Self::on_reload_story`].
    /// `StoryManager::load_active_stories` (StoryManager.cpp:2613-2657).
    pub fn load_active_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_list_id: StoryListId,
        mut promise: Promise<()>,
    ) {
        if !story_list_id.is_valid() {
            promise.set_error(Status::error(400, "Story list must be non-empty"));
            return;
        }
        if self.get_story_list(story_list_id).list_last_story_date == DialogDate::max_dialog_date()
        {
            promise.set_error(Status::error(404, "Not found"));
            return;
        }

        let database_has_more = self.get_story_list(story_list_id).database_has_more;
        if database_has_more {
            self.pending_load_list
                .entry(story_list_id)
                .or_default()
                .push(promise);
            let last_loaded = self
                .get_story_list(story_list_id)
                .last_loaded_database_dialog_date;
            let result = match env.story_db() {
                Some(db) => db.get_active_story_list(
                    story_list_id,
                    last_loaded.get_order(),
                    last_loaded.get_dialog_id(),
                    10,
                ),
                None => StoryDbGetActiveStoryListResult {
                    active_stories: Vec::new(),
                    next_order: last_loaded.get_order(),
                    next_dialog_id: last_loaded.get_dialog_id(),
                },
            };
            self.on_load_active_stories_from_database(env, story_list_id, Result::Ok(result));
            return;
        }

        if !self.get_story_list(story_list_id).server_has_more {
            let story_list = self.get_story_list_mut(story_list_id);
            if story_list.list_last_story_date != DialogDate::max_dialog_date() {
                let min_story_date = story_list.list_last_story_date;
                story_list.list_last_story_date = DialogDate::max_dialog_date();
                let updated_dialog_ids: Vec<DialogId> = story_list
                    .ordered_stories
                    .range((
                        std::ops::Bound::Excluded(min_story_date),
                        std::ops::Bound::Unbounded,
                    ))
                    .map(|date| date.get_dialog_id())
                    .collect();
                for dialog_id in updated_dialog_ids {
                    self.on_dialog_active_stories_order_updated(
                        env,
                        dialog_id,
                        "load_active_stories",
                    );
                }
                self.update_story_list_sent_total_count(env, story_list_id, "load_active_stories");
            }
            promise.set_error(Status::error(404, "Not found"));
            return;
        }

        self.load_active_stories_from_server(env, story_list_id, promise);
    }

    /// `StoryManager::on_load_active_stories_from_database`
    /// (StoryManager.cpp:2659-2728).
    pub fn on_load_active_stories_from_database(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_list_id: StoryListId,
        result: Result<StoryDbGetActiveStoryListResult>,
    ) {
        let Some(promises) = self.pending_load_list.remove(&story_list_id) else {
            return;
        };
        let Result::Ok(active_story_list) = result else {
            for mut promise in promises {
                promise.set_error(result.error().clone());
            }
            return;
        };

        tdutils::log_info!(
            "Load {} chats with active stories from database",
            active_story_list.active_stories.len()
        );

        let mut owner_dialog_ids = std::collections::HashSet::new();
        let mut is_bad = false;
        for (owner_dialog_id, _) in &active_story_list.active_stories {
            if owner_dialog_id.is_valid() {
                env.force_create_dialog(
                    *owner_dialog_id,
                    "on_load_active_stories_from_database",
                    true,
                );
                owner_dialog_ids.insert(*owner_dialog_id);
            } else {
                is_bad = true;
            }
        }
        let mut active_story_list = active_story_list;
        if is_bad {
            active_story_list.active_stories.clear();
            let story_list = self.get_story_list_mut(story_list_id);
            story_list.state.clear();
            story_list.server_has_more = true;
        }

        if active_story_list.active_stories.is_empty() {
            let story_list = self.get_story_list_mut(story_list_id);
            story_list.last_loaded_database_dialog_date = DialogDate::max_dialog_date();
            story_list.database_has_more = false;
        } else {
            for (owner_dialog_id, data) in std::mem::take(&mut active_story_list.active_stories) {
                self.on_get_active_stories_from_database(
                    env,
                    story_list_id,
                    owner_dialog_id,
                    &data,
                    "on_load_active_stories_from_database",
                );
            }
            let max_story_date = DialogDate::new(
                active_story_list.next_order,
                active_story_list.next_dialog_id,
            );
            let story_list = self.get_story_list_mut(story_list_id);
            if story_list.last_loaded_database_dialog_date < max_story_date {
                story_list.last_loaded_database_dialog_date = max_story_date;

                if story_list.list_last_story_date < max_story_date {
                    let min_story_date = story_list.list_last_story_date;
                    story_list.list_last_story_date = max_story_date;
                    let updated_dialog_ids: Vec<DialogId> = story_list
                        .ordered_stories
                        .range((
                            std::ops::Bound::Excluded(min_story_date),
                            std::ops::Bound::Included(max_story_date),
                        ))
                        .map(|date| date.get_dialog_id())
                        .collect();
                    for dialog_id in updated_dialog_ids {
                        owner_dialog_ids.remove(&dialog_id);
                        self.on_dialog_active_stories_order_updated(
                            env,
                            dialog_id,
                            "on_load_active_stories_from_database 1",
                        );
                    }
                    for owner_dialog_id in owner_dialog_ids.clone() {
                        self.on_dialog_active_stories_order_updated(
                            env,
                            owner_dialog_id,
                            "on_load_active_stories_from_database 2",
                        );
                    }
                }
            } else {
                tdutils::log_error!("Last database story date didn't increase");
            }
            self.update_story_list_sent_total_count(
                env,
                story_list_id,
                "on_load_active_stories_from_database",
            );
        }

        for mut promise in promises {
            promise.set_value(());
        }
    }

    /// `StoryManager::load_active_stories_from_server`
    /// (StoryManager.cpp:2730-2743).
    fn load_active_stories_from_server(
        &mut self,
        _env: &mut impl StoryManagerEnv,
        story_list_id: StoryListId,
        promise: Promise<()>,
    ) {
        let queries = self.pending_load_list.entry(story_list_id).or_default();
        let is_first = queries.is_empty();
        queries.push(promise);
        if is_first {
            tdutils::log_info!("Load active stories from server");
            let state = self.get_story_list(story_list_id).state.clone();
            let is_next = !state.is_empty();
            // `GetAllStoriesQuery::send` (StoryManager.cpp:77-84) — the
            // promise lambda's `is_next`/old-state captures park
            // alongside for the answer half.
            self.in_flight_load_list
                .insert(story_list_id, (is_next, state.clone()));
            let mut flags = 0;
            if !state.is_empty() {
                flags |= 1; // telegram_api::stories_getAllStories::STATE_MASK
            }
            self.send_story_net_query(
                StoryNetKind::GetAllStories { story_list_id },
                telegram_api::Function::stories_getAllStories(
                    telegram_api::stories_getAllStories {
                        flags,
                        next: is_next,
                        hidden: story_list_id == StoryListId::Archive,
                        state,
                    },
                ),
                "stories.getAllStories",
            );
        }
    }

    /// `StoryManager::reload_active_stories` (StoryManager.cpp:2745-2748).
    pub fn reload_active_stories(&mut self, env: &mut impl StoryManagerEnv, promise: Promise<()>) {
        self.load_active_stories_from_server(env, StoryListId::Main, promise);
        self.load_active_stories_from_server(env, StoryListId::Archive, Promise::default());
    }

    /// `StoryManager::on_load_active_stories_from_server`
    /// (StoryManager.cpp:2750-2864).
    pub fn on_load_active_stories_from_server(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_list_id: StoryListId,
        is_next: bool,
        old_state: String,
        r_all_stories: Result<Box<telegram_api::stories_AllStories>>,
    ) {
        let Some(promises) = self.pending_load_list.remove(&story_list_id) else {
            return;
        };
        let Result::Ok(all_stories) = r_all_stories else {
            for mut promise in promises {
                promise.set_error(r_all_stories.error().clone());
            }
            return;
        };
        let all_stories = *all_stories;
        match all_stories {
            telegram_api::stories_AllStories::stories_allStoriesNotModified(stories) => {
                if stories.state.is_empty() {
                    tdutils::log_error!("Receive empty state in stories_allStoriesNotModified");
                } else {
                    let story_list = self.get_story_list_mut(story_list_id);
                    story_list.state = stories.state.clone();
                    let (state, server_total_count, server_has_more) = (
                        story_list.state.clone(),
                        story_list.server_total_count,
                        story_list.server_has_more,
                    );
                    self.save_story_list(
                        env,
                        story_list_id,
                        state,
                        server_total_count,
                        server_has_more,
                    );
                }
                self.on_update_story_stealth_mode(env, stories.stealth_mode);
            }
            telegram_api::stories_AllStories::stories_allStories(mut stories) => {
                env.on_get_users(
                    std::mem::take(&mut stories.users),
                    "on_load_active_stories_from_server",
                );
                env.on_get_chats(
                    std::mem::take(&mut stories.chats),
                    "on_load_active_stories_from_server",
                );
                if stories.state.is_empty() {
                    tdutils::log_error!("Receive empty state in stories_allStories");
                } else {
                    let story_list = self.get_story_list_mut(story_list_id);
                    story_list.state = stories.state.clone();
                }
                {
                    let story_list = self.get_story_list_mut(story_list_id);
                    story_list.server_total_count = stories.count.max(0);
                    story_list.is_reloaded_server_total_count = true;
                    if !stories.has_more || stories.peer_stories.is_empty() {
                        story_list.server_has_more = false;
                    }
                }

                if stories.peer_stories.is_empty() && stories.has_more {
                    tdutils::log_error!("Receive no stories, but expected more");
                    stories.has_more = false;
                }

                let mut max_story_date = DialogDate::min_dialog_date();
                let mut owner_dialog_ids = Vec::new();
                for peer_stories in std::mem::take(&mut stories.peer_stories) {
                    let owner_dialog_id = self.on_get_dialog_stories(
                        env,
                        DialogId::default(),
                        Some(peer_stories),
                        Promise::default(),
                    );
                    let Some(active_stories) = self.active_stories.get(&owner_dialog_id) else {
                        tdutils::log_error!("Receive invalid stories");
                        continue;
                    };
                    let story_date = DialogDate::new(active_stories.private_order, owner_dialog_id);
                    if max_story_date < story_date {
                        max_story_date = story_date;
                    } else {
                        tdutils::log_error!(
                            "Receive out-of-order story date for {} request with state \"{old_state}\"",
                            if is_next { "next" } else { "first" }
                        );
                    }
                    owner_dialog_ids.push(owner_dialog_id);
                }
                if !stories.has_more {
                    max_story_date = DialogDate::max_dialog_date();
                }

                let mut delete_dialog_ids = Vec::new();
                let min_story_date = if is_next {
                    self.get_story_list(story_list_id).list_last_story_date
                } else {
                    DialogDate::min_dialog_date()
                };
                {
                    let story_list = self.get_story_list(story_list_id);
                    for date in story_list.ordered_stories.range((
                        std::ops::Bound::Excluded(min_story_date),
                        std::ops::Bound::Included(max_story_date),
                    )) {
                        let dialog_id = date.get_dialog_id();
                        if !owner_dialog_ids.contains(&dialog_id) {
                            delete_dialog_ids.push(dialog_id);
                        }
                    }
                }
                let mut list_updated = false;
                {
                    let story_list = self.get_story_list_mut(story_list_id);
                    if story_list.list_last_story_date < max_story_date {
                        story_list.list_last_story_date = max_story_date;
                        list_updated = true;
                    } else if is_next {
                        tdutils::log_error!("Last story date didn't increase");
                    }
                }
                if list_updated {
                    for owner_dialog_id in &owner_dialog_ids {
                        self.on_dialog_active_stories_order_updated(
                            env,
                            *owner_dialog_id,
                            "on_load_active_stories_from_server",
                        );
                    }
                }
                for dialog_id in delete_dialog_ids {
                    self.on_update_active_stories(
                        env,
                        dialog_id,
                        StoryId::default(),
                        Vec::new(),
                        Promise::default(),
                        "on_load_active_stories_from_server",
                        false,
                    );
                    self.load_dialog_expiring_stories(
                        env,
                        dialog_id,
                        "on_load_active_stories_from_server 1",
                    );
                }
                self.update_story_list_sent_total_count(
                    env,
                    story_list_id,
                    "on_load_active_stories_from_server",
                );

                self.on_update_story_stealth_mode(env, stories.stealth_mode);
            }
        }

        for mut promise in promises {
            promise.set_value(());
        }
    }

    /// `StoryManager::on_dialog_active_stories_order_updated`
    /// (StoryManager.cpp:5507-5519).
    pub fn on_dialog_active_stories_order_updated(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        source: &str,
    ) {
        let mut need_save_to_database = false;
        let updated = if self.active_stories.contains_key(&owner_dialog_id) {
            self.update_active_stories_order(env, owner_dialog_id, &mut need_save_to_database)
        } else {
            false
        };
        if updated {
            let active_stories = self.active_stories.get(&owner_dialog_id).cloned();
            self.send_update_chat_active_stories(
                env,
                owner_dialog_id,
                active_stories.as_ref(),
                source,
            );
        }
        if need_save_to_database {
            let active_stories = self.active_stories.get(&owner_dialog_id).cloned();
            self.save_active_stories(
                env,
                owner_dialog_id,
                active_stories.as_ref(),
                Promise::default(),
                source,
            );
        }
    }

    /// `StoryManager::toggle_dialog_stories_hidden`
    /// (StoryManager.cpp:2973-2986).
    pub fn toggle_dialog_stories_hidden(
        &mut self,
        env: &mut impl StoryManagerEnv,
        dialog_id: DialogId,
        story_list_id: StoryListId,
        mut promise: Promise<()>,
    ) {
        let mut status =
            env.check_dialog_access(dialog_id, false, false, "toggle_dialog_stories_hidden");
        if status.is_error() {
            promise.set_error(status.move_as_error());
            return;
        }
        if story_list_id == self.get_dialog_story_list_id(env, dialog_id) {
            promise.set_value(());
            return;
        }
        if !story_list_id.is_valid() {
            promise.set_error(Status::error(400, "Story list must be non-empty"));
            return;
        }

        // `ToggleStoriesHiddenQuery::send` (StoryManager.cpp:111-118) —
        // the Read peer (the C++ CHECK; the access gate above guarantees
        // it) rides with the hidden flag.
        let are_hidden = story_list_id == StoryListId::Archive;
        let input_peer = env
            .get_input_peer(dialog_id, crate::user_manager::AccessRights::Read)
            .expect("the access gate guaranteed the peer");
        self.pending_toggle_stories_hidden.push(promise);
        self.send_story_net_query(
            StoryNetKind::ToggleStoriesHidden {
                dialog_id,
                are_hidden,
            },
            telegram_api::Function::stories_togglePeerStoriesHidden(
                telegram_api::stories_togglePeerStoriesHidden {
                    peer: Some(Box::new(input_peer)),
                    hidden: are_hidden,
                },
            ),
            "stories.togglePeerStoriesHidden",
        );
    }

    /// The ToggleStoriesHiddenQuery true-Bool answer
    /// (StoryManager.cpp:120-132) — the hidden flip lands through
    /// `on_update_dialog_stories_hidden`; the parked family promise
    /// settles (`on_story_net_answer` pops it).
    pub fn on_toggle_dialog_stories_hidden(
        &mut self,
        env: &mut impl StoryManagerEnv,
        dialog_id: DialogId,
        are_hidden: bool,
        mut promise: Promise<()>,
    ) {
        self.on_update_dialog_stories_hidden(env, dialog_id, are_hidden);
        promise.set_value(());
    }

    /// `StoryManager::get_dialog_pinned_stories`
    /// (StoryManager.cpp:2988-3013).
    pub fn get_dialog_pinned_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        from_story_id: StoryId,
        limit: i32,
        mut promise: Promise<td_api::Stories>,
    ) {
        if limit <= 0 {
            promise.set_error(Status::error(400, "Parameter limit must be positive"));
            return;
        }
        let mut status =
            env.check_dialog_access(owner_dialog_id, false, false, "get_dialog_pinned_stories");
        if status.is_error() {
            promise.set_error(status.move_as_error());
            return;
        }

        if from_story_id != StoryId::default() && !from_story_id.is_server() {
            promise.set_error(Status::error(
                400,
                "Invalid value of parameter from_story_id specified",
            ));
            return;
        }
        if !self.can_have_stories(env, owner_dialog_id) {
            promise.set_value(td_api::Stories::default());
            return;
        }

        self.pending_pinned_stories
            .entry(owner_dialog_id)
            .or_default()
            .push(promise);
        env.send_get_pinned_stories_query(owner_dialog_id, from_story_id, limit);
    }

    /// `StoryManager::on_get_dialog_pinned_stories`
    /// (StoryManager.cpp:3015-3026).
    pub fn on_get_dialog_pinned_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        stories: Box<telegram_api::stories_Stories>,
    ) {
        let pinned_story_ids = StoryId::get_story_ids(&stories.pinned_to_top);
        let (total_count, story_ids) =
            self.on_get_stories(env, owner_dialog_id, Vec::new(), stories);
        self.on_update_dialog_has_pinned_stories(env, owner_dialog_id, total_count > 0);
        let story_full_ids: Vec<StoryFullId> = story_ids
            .iter()
            .map(|story_id| StoryFullId::new(owner_dialog_id, *story_id))
            .collect();
        let result = self.get_stories_object(env, total_count, &story_full_ids, &pinned_story_ids);
        if let Some(promises) = self.pending_pinned_stories.remove(&owner_dialog_id) {
            for mut promise in promises {
                promise.set_value(result.clone());
            }
        }
    }

    /// `StoryManager::get_story_archive` (StoryManager.cpp:3028-3053).
    pub fn get_story_archive(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        from_story_id: StoryId,
        limit: i32,
        mut promise: Promise<td_api::Stories>,
    ) {
        if limit <= 0 {
            promise.set_error(Status::error(400, "Parameter limit must be positive"));
            return;
        }
        if from_story_id != StoryId::default() && !from_story_id.is_server() {
            promise.set_error(Status::error(
                400,
                "Invalid value of parameter from_story_id specified",
            ));
            return;
        }
        if !env.have_dialog_force(owner_dialog_id, "get_story_archive") {
            promise.set_error(Status::error(400, "Chat not found"));
            return;
        }
        if !self.can_edit_stories(env, owner_dialog_id) {
            promise.set_error(Status::error(400, "Can't get story archive in the chat"));
            return;
        }

        self.pending_archive_stories
            .entry(owner_dialog_id)
            .or_default()
            .push(promise);
        env.send_get_stories_archive_query(owner_dialog_id, from_story_id, limit);
    }

    /// `StoryManager::on_get_story_archive` (StoryManager.cpp:3055-3065).
    pub fn on_get_story_archive(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        stories: Box<telegram_api::stories_Stories>,
    ) {
        if !stories.pinned_to_top.is_empty() {
            tdutils::log_error!("Receive pinned stories in archive");
        }
        let (total_count, story_ids) =
            self.on_get_stories(env, owner_dialog_id, Vec::new(), stories);
        let story_full_ids: Vec<StoryFullId> = story_ids
            .iter()
            .map(|story_id| StoryFullId::new(owner_dialog_id, *story_id))
            .collect();
        let result = self.get_stories_object(env, total_count, &story_full_ids, &[]);
        if let Some(promises) = self.pending_archive_stories.remove(&owner_dialog_id) {
            for mut promise in promises {
                promise.set_value(result.clone());
            }
        }
    }

    /// `StoryManager::get_dialog_expiring_stories`
    /// (StoryManager.cpp:3067-3098).
    pub fn get_dialog_expiring_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        mut promise: Promise<td_api::ChatActiveStories>,
    ) {
        let mut status =
            env.check_dialog_access(owner_dialog_id, false, false, "get_dialog_expiring_stories");
        if status.is_error() {
            promise.set_error(status.move_as_error());
            return;
        }

        tdutils::log_info!("Get active stories in dialog {}", owner_dialog_id.get());
        let loaded = self
            .get_active_stories_force(env, owner_dialog_id, "get_dialog_expiring_stories")
            .is_some();
        if loaded {
            if self.updated_active_stories.insert(owner_dialog_id) {
                let active_stories = self.active_stories.get(&owner_dialog_id).cloned();
                self.send_update_chat_active_stories(
                    env,
                    owner_dialog_id,
                    active_stories.as_ref(),
                    "get_dialog_expiring_stories 2",
                );
            }
            let active_stories = self.active_stories.get(&owner_dialog_id).cloned();
            promise.set_value(self.get_chat_active_stories_object(
                env,
                owner_dialog_id,
                active_stories.as_ref(),
            ));
            return;
        }
        if !self.can_have_stories(env, owner_dialog_id) {
            promise.set_value(td_api::ChatActiveStories::default());
            return;
        }

        self.pending_expiring_stories
            .entry(owner_dialog_id)
            .or_default()
            .push(promise);
        // `GetPeerStoriesQuery::send` (StoryManager.cpp:513-521) — the
        // Read peer; the null band is the C++ on_error head plus the
        // query-promise lambda's error arm (the parked promises fail).
        match env.get_input_peer(owner_dialog_id, crate::user_manager::AccessRights::Read) {
            Some(input_peer) => self.send_story_net_query(
                StoryNetKind::GetPeerStories {
                    dialog_id: owner_dialog_id,
                },
                telegram_api::Function::stories_getPeerStories(
                    telegram_api::stories_getPeerStories {
                        peer: Some(Box::new(input_peer)),
                    },
                ),
                "stories.getPeerStories",
            ),
            None => {
                let status = Status::error(400, "Can't access the chat");
                env.on_get_dialog_error(owner_dialog_id, &status, "GetPeerStoriesQuery");
                if let Some(promises) = self.pending_expiring_stories.remove(&owner_dialog_id) {
                    for mut promise in promises {
                        promise.set_error(status.clone());
                    }
                }
            }
        }
    }

    /// `StoryManager::on_get_dialog_expiring_stories`
    /// (StoryManager.cpp:3169-3186).
    pub fn on_get_dialog_expiring_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        stories: telegram_api::stories_PeerStories,
    ) {
        env.on_get_users(stories.users, "on_get_dialog_expiring_stories");
        env.on_get_chats(stories.chats, "on_get_dialog_expiring_stories");
        let owner_dialog_id =
            self.on_get_dialog_stories(env, owner_dialog_id, stories.stories, Promise::default());
        let active_stories = self.active_stories.get(&owner_dialog_id).cloned();
        if self.updated_active_stories.insert(owner_dialog_id) {
            self.send_update_chat_active_stories(
                env,
                owner_dialog_id,
                active_stories.as_ref(),
                "on_get_dialog_expiring_stories",
            );
        }
        let result =
            self.get_chat_active_stories_object(env, owner_dialog_id, active_stories.as_ref());
        if let Some(promises) = self.pending_expiring_stories.remove(&owner_dialog_id) {
            for mut promise in promises {
                promise.set_value(result.clone());
            }
        }
    }

    /// `save_load_dialog_expiring_stories_log_event` — [blocked: binlog].
    fn save_load_dialog_expiring_stories_log_event(&mut self, owner_dialog_id: DialogId) -> u64 {
        let _ = owner_dialog_id;
        0
    }

    /// `StoryManager::load_dialog_expiring_stories`
    /// (StoryManager.cpp:3129-3151).
    pub fn load_dialog_expiring_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        source: &str,
    ) {
        if self
            .load_expiring_stories_dialog_ids
            .contains(&owner_dialog_id)
            || !self.can_have_stories(env, owner_dialog_id)
        {
            return;
        }

        tdutils::log_info!(
            "Load active stories in dialog {} from {source}",
            owner_dialog_id.get()
        );
        let _ = self.save_load_dialog_expiring_stories_log_event(owner_dialog_id);
        self.load_expiring_stories_dialog_ids
            .insert(owner_dialog_id);

        // the C++ hops through get_dialog_expiring_stories with a parked
        // object promise; the Rust port answers the parked chatActiveStories
        // promise list and then clears the load state
        let parked = self.pending_expiring_stories.remove(&owner_dialog_id);
        let active_stories = self.active_stories.get(&owner_dialog_id).cloned();
        let result =
            self.get_chat_active_stories_object(env, owner_dialog_id, active_stories.as_ref());
        if let Some(promises) = parked {
            for mut promise in promises {
                promise.set_value(result.clone());
            }
        }
        // `GetPeerStoriesQuery::send` (StoryManager.cpp:513-521) — the
        // reload leg's Read peer; the null band runs the C++ on_error
        // head only (the hop's promises settled above).
        match env.get_input_peer(owner_dialog_id, crate::user_manager::AccessRights::Read) {
            Some(input_peer) => self.send_story_net_query(
                StoryNetKind::GetPeerStories {
                    dialog_id: owner_dialog_id,
                },
                telegram_api::Function::stories_getPeerStories(
                    telegram_api::stories_getPeerStories {
                        peer: Some(Box::new(input_peer)),
                    },
                ),
                "stories.getPeerStories",
            ),
            None => {
                let status = Status::error(400, "Can't access the chat");
                env.on_get_dialog_error(owner_dialog_id, &status, "GetPeerStoriesQuery");
            }
        }
    }

    /// `StoryManager::on_load_dialog_expiring_stories`
    /// (StoryManager.cpp:3153-3167).
    pub fn on_load_dialog_expiring_stories(&mut self, owner_dialog_id: DialogId) {
        if self
            .load_expiring_stories_dialog_ids
            .remove(&owner_dialog_id)
        {
            tdutils::log_info!(
                "Finished loading of active stories in dialog {}",
                owner_dialog_id.get()
            );
        }
    }

    /// `StoryManager::reload_dialog_expiring_stories`
    /// (StoryManager.cpp:3100-3106).
    pub fn reload_dialog_expiring_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        dialog_id: DialogId,
    ) {
        if !env.have_input_peer(dialog_id, false, false) || !self.can_have_stories(env, dialog_id) {
            return;
        }
        env.force_create_dialog(dialog_id, "reload_dialog_expiring_stories", false);
        self.load_dialog_expiring_stories(env, dialog_id, "reload_dialog_expiring_stories");
    }

    /// `StoryManager::search_hashtag_posts` (StoryManager.cpp:3188-3214).
    pub fn search_hashtag_posts(
        &mut self,
        env: &mut impl StoryManagerEnv,
        dialog_id: DialogId,
        mut hashtag: String,
        offset: String,
        mut limit: i32,
        mut promise: Promise<td_api::FoundStories>,
    ) {
        if limit <= 0 {
            promise.set_error(Status::error(400, "Parameter limit must be positive"));
            return;
        }
        if limit > MAX_SEARCH_STORIES {
            limit = MAX_SEARCH_STORIES;
        }
        if dialog_id != DialogId::default() {
            let mut status =
                env.check_dialog_access(dialog_id, false, false, "search_hashtag_posts");
            if status.is_error() {
                promise.set_error(status.move_as_error());
                return;
            }
        }

        let mut query_prefix = '#';
        if hashtag.starts_with('#') || hashtag.starts_with('$') {
            query_prefix = hashtag.remove(0);
        }
        if hashtag.is_empty() {
            promise.set_value(td_api::FoundStories::default());
            return;
        }

        let query = format!("{query_prefix}{hashtag}");
        self.pending_search_stories.push(promise);
        env.send_search_stories_query(
            dialog_id,
            SearchStoriesArm::Hashtag { query: &query },
            &offset,
            limit,
        );
    }

    /// `StoryManager::search_location_posts` (StoryManager.cpp:3216-3226).
    pub fn search_location_posts(
        &mut self,
        env: &mut impl StoryManagerEnv,
        address: Option<td_api::LocationAddress>,
        offset: String,
        mut limit: i32,
        mut promise: Promise<td_api::FoundStories>,
    ) {
        if limit <= 0 {
            promise.set_error(Status::error(400, "Parameter limit must be positive"));
            return;
        }
        if limit > MAX_SEARCH_STORIES {
            limit = MAX_SEARCH_STORIES;
        }
        if address.is_none() {
            promise.set_error(Status::error(400, "Address must be non-empty"));
            return;
        }
        let address = address.expect("checked above");

        self.pending_search_stories.push(promise);
        env.send_search_stories_query(
            DialogId::default(),
            SearchStoriesArm::Location { address: &address },
            &offset,
            limit,
        );
    }

    /// `StoryManager::search_venue_posts` (StoryManager.cpp:3228-3238).
    pub fn search_venue_posts(
        &mut self,
        env: &mut impl StoryManagerEnv,
        venue_provider: String,
        venue_id: String,
        offset: String,
        mut limit: i32,
        mut promise: Promise<td_api::FoundStories>,
    ) {
        if limit <= 0 {
            promise.set_error(Status::error(400, "Parameter limit must be positive"));
            return;
        }
        if limit > MAX_SEARCH_STORIES {
            limit = MAX_SEARCH_STORIES;
        }

        self.pending_search_stories.push(promise);
        env.send_search_stories_query(
            DialogId::default(),
            SearchStoriesArm::Venue {
                venue_provider: &venue_provider,
                venue_id: &venue_id,
            },
            &offset,
            limit,
        );
    }

    /// `StoryManager::set_pinned_stories` (StoryManager.cpp:3264-3284).
    pub fn set_pinned_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_ids: Vec<StoryId>,
        mut promise: Promise<()>,
    ) {
        let mut status =
            env.check_dialog_access(owner_dialog_id, false, true, "set_pinned_stories");
        if status.is_error() {
            promise.set_error(status.move_as_error());
            return;
        }
        if !self.can_edit_stories(env, owner_dialog_id) {
            promise.set_error(Status::error(
                400,
                "Can't change pinned stories in the chat",
            ));
            return;
        }
        for story_id in &story_ids {
            let story_full_id = StoryFullId::new(owner_dialog_id, *story_id);
            let Some(story) = self.get_story(story_full_id) else {
                promise.set_error(Status::error(400, "Story not found"));
                return;
            };
            if !story.is_pinned {
                promise.set_error(Status::error(
                    400,
                    "The story must be posted to the chat page first",
                ));
                return;
            }
            if !story_id.is_server() {
                promise.set_error(Status::error(400, "Story must be sent first"));
                return;
            }
        }
        // `TogglePinnedStoriesToTopQuery::send`
        // (StoryManager.cpp:829-838) — the Write peer (the C++ CHECK; the
        // validation above guarantees the stories); the answer settles
        // without reading the Updates body.
        let input_peer = env
            .get_input_peer(owner_dialog_id, crate::user_manager::AccessRights::Write)
            .expect("the pinned-stories gate guaranteed the peer");
        self.pending_set_pinned_stories.push(promise);
        self.send_story_net_query(
            StoryNetKind::TogglePinnedStoriesToTop {
                dialog_id: owner_dialog_id,
            },
            telegram_api::Function::stories_togglePinnedToTop(
                telegram_api::stories_togglePinnedToTop {
                    peer: Some(Box::new(input_peer)),
                    id: story_ids.iter().map(|story_id| story_id.get()).collect(),
                },
            ),
            "stories.togglePinnedToTop",
        );
    }

    /// `StoryManager::get_story_interactions` (StoryManager.cpp:3676-3704).
    /// The C++-faithful argument list (the convention's
    /// too_many_arguments note).
    #[allow(clippy::too_many_arguments)]
    pub fn get_story_interactions(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_id: StoryId,
        query: &str,
        only_contacts: bool,
        prefer_forwards: bool,
        prefer_with_reaction: bool,
        offset: &str,
        limit: i32,
        mut promise: Promise<td_api::StoryInteractions>,
    ) {
        let owner_dialog_id = env.get_my_dialog_id();
        let story_full_id = StoryFullId::new(owner_dialog_id, story_id);
        if self.get_story(story_full_id).is_none() {
            promise.set_error(Status::error(400, "Story not found"));
            return;
        }
        if limit <= 0 {
            promise.set_error(Status::error(400, "Parameter limit must be positive"));
            return;
        }
        if !story_id.is_server() {
            promise.set_value(td_api::StoryInteractions::default());
            return;
        }

        let is_full = query.is_empty() && !only_contacts;
        let _ = (is_full, offset);
        self.pending_story_interactions.push(promise);
        env.send_get_story_views_list_query(
            owner_dialog_id,
            story_id,
            query,
            only_contacts,
            prefer_forwards,
            prefer_with_reaction,
            offset,
            limit,
        );
    }

    /// `StoryManager::on_get_story_interactions`
    /// (StoryManager.cpp:3706-3754).
    /// The C++ nested-guard style (the convention's collapsible-if
    /// note).
    #[allow(clippy::collapsible_if)]
    pub fn on_get_story_interactions(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_id: StoryId,
        is_full: bool,
        is_first: bool,
        r_view_list: Result<Box<telegram_api::stories_StoryViewsList>>,
    ) {
        let Result::Ok(view_list) = r_view_list else {
            let status = r_view_list.error().clone();
            let promises = std::mem::take(&mut self.pending_story_interactions);
            for mut promise in promises {
                promise.set_error(status.clone());
            }
            return;
        };
        let mut view_list = *view_list;

        let owner_dialog_id = env.get_my_dialog_id();
        let story_full_id = StoryFullId::new(owner_dialog_id, story_id);

        let mut total_count = view_list.count;
        if total_count < 0 || total_count < view_list.views.len() as i32 {
            tdutils::log_error!("Receive invalid total_count {total_count}");
            total_count = view_list.views.len() as i32;
        }
        let mut total_reaction_count = view_list.reactions_count;
        if total_reaction_count < 0 || total_reaction_count > total_count {
            tdutils::log_error!("Receive invalid total_reaction_count {total_reaction_count}");
            total_reaction_count = total_count;
        }
        let total_forward_count = view_list.forwards_count.max(0);

        let story_viewers = crate::story_viewer::StoryViewers::from_story_views(
            self,
            env,
            total_count,
            total_forward_count,
            total_reaction_count,
            std::mem::take(&mut view_list.views),
            std::mem::take(&mut view_list.next_offset),
        );
        let story = self.stories.get_mut(&story_full_id);
        if let Some(story) = story {
            if story.content.is_some() {
                let mut is_changed = false;
                if is_full {
                    let mut view_count = story.interaction_info.get_view_count();
                    let mut reaction_count = story.interaction_info.get_reaction_count();
                    if story
                        .interaction_info
                        .set_counts(&mut view_count, &mut reaction_count)
                    {
                        is_changed = true;
                    }
                }
                let _ = is_first;
                if is_changed {
                    let story = story.clone();
                    self.on_story_changed(env, story_full_id, &story, true, true, false);
                }
            }
        }

        let result = story_viewers.get_story_interactions_object(self, env);
        let promises = std::mem::take(&mut self.pending_story_interactions);
        for mut promise in promises {
            promise.set_value(result.clone());
        }
    }

    /// `StoryManager::get_dialog_story_interactions`
    /// (StoryManager.cpp:3783-3809).
    /// The C++-faithful argument list (the convention's
    /// too_many_arguments note).
    #[allow(clippy::too_many_arguments)]
    pub fn get_dialog_story_interactions(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
        reaction_type: ReactionType,
        prefer_forwards: bool,
        offset: &str,
        limit: i32,
        mut promise: Promise<td_api::StoryInteractions>,
    ) {
        if self.get_story(story_full_id).is_none() {
            promise.set_error(Status::error(400, "Story not found"));
            return;
        }
        if limit <= 0 {
            promise.set_error(Status::error(400, "Parameter limit must be positive"));
            return;
        }
        if !story_full_id.get_story_id().is_server() {
            promise.set_value(td_api::StoryInteractions::default());
            return;
        }
        if reaction_type.is_paid_reaction() {
            promise.set_error(Status::error(400, "Stories can't have paid reactions"));
            return;
        }

        self.pending_dialog_story_interactions.push(promise);
        env.send_get_story_reactions_list_query(
            story_full_id,
            &reaction_type,
            prefer_forwards,
            offset,
            limit,
        );
    }

    /// `StoryManager::on_get_dialog_story_interactions`
    /// (StoryManager.cpp:3811-3837).
    pub fn on_get_dialog_story_interactions(
        &mut self,
        env: &mut impl StoryManagerEnv,
        _story_full_id: StoryFullId,
        r_reaction_list: Result<Box<telegram_api::stories_StoryReactionsList>>,
    ) {
        let Result::Ok(reaction_list) = r_reaction_list else {
            let status = r_reaction_list.error().clone();
            let promises = std::mem::take(&mut self.pending_dialog_story_interactions);
            for mut promise in promises {
                promise.set_error(status.clone());
            }
            return;
        };
        let mut reaction_list = *reaction_list;

        let mut total_count = reaction_list.count;
        if total_count < 0 || total_count < reaction_list.reactions.len() as i32 {
            tdutils::log_error!("Receive invalid total_count {total_count}");
            total_count = reaction_list.reactions.len() as i32;
        }

        let story_viewers = crate::story_viewer::StoryViewers::from_story_reactions(
            self,
            env,
            total_count,
            std::mem::take(&mut reaction_list.reactions),
            std::mem::take(&mut reaction_list.next_offset),
        );
        let result = story_viewers.get_story_interactions_object(self, env);
        let promises = std::mem::take(&mut self.pending_dialog_story_interactions);
        for mut promise in promises {
            promise.set_value(result.clone());
        }
    }

    /// `StoryManager::report_story` (StoryManager.cpp:3839-3849).
    pub fn report_story(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
        option_id: &str,
        text: &str,
        mut promise: Promise<td_api::ReportStoryResult>,
    ) {
        if !self.have_story_force(env, story_full_id) {
            promise.set_error(Status::error(400, "Story not found"));
            return;
        }
        if !story_full_id.is_server() {
            promise.set_error(Status::error(400, "Story can't be reported"));
            return;
        }

        self.pending_report_story.push(promise);
        // `ReportStoryQuery::send` (StoryManager.cpp:895-906) — the Read
        // peer, the single id, the raw-bytes option and the text; the
        // null band is the C++ on_error head + the promise error.
        let owner_dialog_id = story_full_id.get_dialog_id();
        let Some(input_peer) =
            env.get_input_peer(owner_dialog_id, crate::user_manager::AccessRights::Read)
        else {
            let status = Status::error(400, "Can't access the chat");
            env.on_get_dialog_error(owner_dialog_id, &status, "ReportStoryQuery");
            if let Some(mut promise) = self.pending_report_story.pop() {
                promise.set_error(status);
            }
            return;
        };
        self.send_story_net_query(
            StoryNetKind::ReportStory {
                dialog_id: owner_dialog_id,
            },
            telegram_api::Function::stories_report(telegram_api::stories_report {
                peer: Some(Box::new(input_peer)),
                id: vec![story_full_id.get_story_id().get()],
                option: option_id.as_bytes().to_vec(),
                message: text.to_owned(),
            }),
            "stories.report",
        );
    }

    /// `StoryManager::get_story_albums` (StoryManager.cpp:3851-3856).
    pub fn get_story_albums(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        mut promise: Promise<td_api::StoryAlbums>,
    ) {
        let mut status = env.check_dialog_access(owner_dialog_id, false, false, "get_story_albums");
        if status.is_error() {
            promise.set_error(status.move_as_error());
            return;
        }
        self.pending_story_albums
            .entry(owner_dialog_id)
            .or_default()
            .push(promise);
        env.send_get_story_albums_query(owner_dialog_id);
    }

    /// The GetStoryAlbumsQuery answer (StoryManager.cpp) — the albums land
    /// through `StoryAlbum::from_telegram` + `register_story_album`.
    pub fn on_get_story_albums(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        albums: Vec<telegram_api::StoryAlbum>,
    ) {
        let mut album_objects = Vec::new();
        for story_album in albums {
            let album =
                crate::story_album::StoryAlbum::from_telegram(env, owner_dialog_id, story_album);
            if !album.is_valid() {
                continue;
            }
            let album_full_id = crate::story_ids::StoryAlbumFullId::new(
                owner_dialog_id,
                album.get_story_album_id(),
            );
            self.register_story_album(env, album_full_id, &album);
            album_objects.push(album.get_story_album_object(env));
        }
        let result = td_api::StoryAlbums {
            albums: album_objects,
        };
        if let Some(promises) = self.pending_story_albums.remove(&owner_dialog_id) {
            for mut promise in promises {
                promise.set_value(result.clone());
            }
        }
    }

    /// The GetStoryAlbumsQuery `albumsNotModified` answer
    /// (StoryManager.cpp:982-984) — the empty album list value.
    pub(crate) fn on_get_story_albums_not_modified(&mut self, owner_dialog_id: DialogId) {
        let result = td_api::StoryAlbums::default();
        if let Some(promises) = self.pending_story_albums.remove(&owner_dialog_id) {
            for mut promise in promises {
                promise.set_value(result.clone());
            }
        }
    }

    // ---- the net-drain 20 answer halves' manager legs (the C++
    // ResultHandler on_result/on_error bodies of the §3.5 slice; the
    // parse + the wire-error heads live in `Td::on_story_net_answer`) ----

    /// The GetPinnedStoriesQuery error tail (StoryManager.cpp:461-465) —
    /// the parked promises fail with the status.
    pub(crate) fn fail_pinned_stories(&mut self, dialog_id: DialogId, status: Status) {
        if let Some(promises) = self.pending_pinned_stories.remove(&dialog_id) {
            for mut promise in promises {
                promise.set_error(status.clone());
            }
        }
    }

    /// The GetStoriesArchiveQuery error tail (StoryManager.cpp:497-501).
    pub(crate) fn fail_archive_stories(&mut self, dialog_id: DialogId, status: Status) {
        if let Some(promises) = self.pending_archive_stories.remove(&dialog_id) {
            for mut promise in promises {
                promise.set_error(status.clone());
            }
        }
    }

    /// The GetStoryAlbumsQuery error tail (StoryManager.cpp:993-997).
    pub(crate) fn fail_story_albums(&mut self, owner_dialog_id: DialogId, status: Status) {
        if let Some(promises) = self.pending_story_albums.remove(&owner_dialog_id) {
            for mut promise in promises {
                promise.set_error(status.clone());
            }
        }
    }

    /// The GetAlbumStoriesQuery error tail (StoryManager.cpp:1027-1031).
    pub(crate) fn fail_album_stories(
        &mut self,
        dialog_id: DialogId,
        story_album_id: StoryAlbumId,
        status: Status,
    ) {
        if let Some(promises) = self
            .pending_album_stories
            .remove(&(dialog_id, story_album_id))
        {
            for mut promise in promises {
                promise.set_error(status.clone());
            }
        }
    }

    /// The GetStoriesViewsQuery context pop — the handler's
    /// `dialog_id_`/`story_ids_` members for the answer intake.
    pub(crate) fn take_get_stories_views_context(&mut self) -> Option<(DialogId, Vec<StoryId>)> {
        if self.pending_get_stories_views.is_empty() {
            None
        } else {
            Some(self.pending_get_stories_views.remove(0))
        }
    }

    /// The SearchStoriesQuery error tail (StoryManager.cpp:805-815) —
    /// the `SEARCH_QUERY_EMPTY` band answers the empty value, the rest
    /// fail the parked promises.
    pub(crate) fn on_search_stories_error(&mut self, status: Status) {
        let promises = std::mem::take(&mut self.pending_search_stories);
        if status.message() == "SEARCH_QUERY_EMPTY" {
            for mut promise in promises {
                promise.set_value(td_api::FoundStories::default());
            }
            return;
        }
        for mut promise in promises {
            promise.set_error(status.clone());
        }
    }

    /// The SearchStoriesQuery answer (StoryManager.cpp:786-804) — the
    /// users/chats land through the real intakes, every found story
    /// through `on_get_story`, and the assembled `foundStories` value
    /// settles the parked promises.
    #[allow(clippy::boxed_local)] // the codegen boxes the found umbrella
    pub(crate) fn on_search_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        mut found_stories: Box<telegram_api::stories_FoundStories>,
    ) {
        env.on_get_users(
            std::mem::take(&mut found_stories.users),
            "SearchStoriesQuery",
        );
        env.on_get_chats(
            std::mem::take(&mut found_stories.chats),
            "SearchStoriesQuery",
        );

        let mut total_count = found_stories.count;
        if total_count < found_stories.stories.len() as i32 {
            tdutils::log_error!(
                "Receive total count = {total_count} and {} stories",
                found_stories.stories.len()
            );
            total_count = found_stories.stories.len() as i32;
        }
        let mut stories = Vec::new();
        for found_story in found_stories.stories.iter() {
            let owner_dialog_id = DialogId::from_peer(
                found_story
                    .peer
                    .as_ref()
                    .unwrap_or(&telegram_api::Peer::default()),
            );
            let story = found_story.story.clone().map(|boxed| *boxed);
            let story_id = self.on_get_story(env, owner_dialog_id, story);
            if story_id.is_valid() {
                let story_object =
                    self.get_story_object(env, StoryFullId::new(owner_dialog_id, story_id));
                match story_object {
                    Some(story_object) => stories.push(story_object),
                    None => {
                        tdutils::log_error!("Receive deleted {story_id} from {owner_dialog_id:?}")
                    }
                }
            }
        }
        let result = td_api::FoundStories {
            total_count,
            stories,
            next_offset: std::mem::take(&mut found_stories.next_offset),
        };
        let promises = std::mem::take(&mut self.pending_search_stories);
        for mut promise in promises {
            promise.set_value(result.clone());
        }
    }

    /// The CreateStoryAlbumQuery error tail (StoryManager.cpp:1067-1071)
    /// — the parked promise + context fail with the status.
    pub(crate) fn on_create_story_album_error(&mut self, status: Status) {
        if self.pending_create_story_album_contexts.is_empty() {
            return;
        }
        self.pending_create_story_album_contexts.remove(0);
        let mut promise = self.pending_create_story_albums.remove(0);
        promise.set_error(status);
    }

    /// The CreateStoryAlbumQuery answer (StoryManager.cpp:1047-1066) —
    /// the invalid-album 500 band runs the dialog-error head like the
    /// C++ `on_error` re-entry; the ok band registers the album, adds
    /// the stories and settles the promise with the album object.
    pub(crate) fn on_create_story_album(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_album: telegram_api::StoryAlbum,
    ) {
        let Some((owner_dialog_id, story_ids)) =
            self.pending_create_story_album_contexts.first().cloned()
        else {
            return;
        };
        let album =
            crate::story_album::StoryAlbum::from_telegram(env, owner_dialog_id, story_album);
        if !album.is_valid() {
            let status = Status::error(500, "Receive invalid album");
            env.on_get_dialog_error(owner_dialog_id, &status, "CreateStoryAlbumQuery");
            self.on_create_story_album_error(status);
            return;
        }
        self.update_story_albums(
            env,
            owner_dialog_id,
            &story_ids,
            album.get_story_album_id(),
            true,
        );
        let album_object = album.get_story_album_object(env);
        self.pending_create_story_album_contexts.remove(0);
        let mut promise = self.pending_create_story_albums.remove(0);
        promise.set_value(album_object);
    }

    /// The ReorderStoryAlbumsQuery/DeleteStoryAlbumQuery Bool answer —
    /// the parked unit promises settle (the shared queue, FIFO per kind
    /// — the §5 shape).
    pub(crate) fn on_story_album_unit_ok(&mut self) {
        let promises = std::mem::take(&mut self.pending_story_album_unit_queries);
        for mut promise in promises {
            promise.set_value(());
        }
    }

    /// The ReorderStoryAlbumsQuery error tail (StoryManager.cpp:1088-1092)
    /// / the DeleteStoryAlbumQuery one (StoryManager.cpp:1127-1131).
    pub(crate) fn on_story_album_unit_error(&mut self, status: Status) {
        let promises = std::mem::take(&mut self.pending_story_album_unit_queries);
        for mut promise in promises {
            promise.set_error(status.clone());
        }
    }

    /// The UpdateStoryAlbumQuery error tail (StoryManager.cpp:1230-1240)
    /// — the optimistic local album-story moves roll back
    /// (`update_story_albums` with the handler members) before the
    /// promise fails.
    pub(crate) fn on_update_story_album_error(
        &mut self,
        env: &mut impl StoryManagerEnv,
        status: Status,
    ) {
        if self.pending_update_story_album_contexts.is_empty() {
            return;
        }
        let (owner_dialog_id, story_album_id, deleted_story_ids, added_story_ids) =
            self.pending_update_story_album_contexts.remove(0);
        self.update_story_albums(
            env,
            owner_dialog_id,
            &deleted_story_ids,
            story_album_id,
            true,
        );
        self.update_story_albums(
            env,
            owner_dialog_id,
            &added_story_ids,
            story_album_id,
            false,
        );
        let mut promise = self.pending_update_story_albums.remove(0);
        promise.set_error(status);
    }

    /// The front UpdateStoryAlbum context — the wire-error head reads
    /// the owner dialog before the tail pops it.
    pub(crate) fn peek_update_story_album_context(
        &mut self,
    ) -> Option<(DialogId, StoryAlbumId, Vec<StoryId>, Vec<StoryId>)> {
        self.pending_update_story_album_contexts.first().cloned()
    }

    /// The UpdateStoryAlbumQuery answer (StoryManager.cpp:1213-1229) —
    /// the id-mismatch band replays the error tail (the C++ `on_error`
    /// re-entry with the 500), the ok band settles with the server
    /// album object.
    pub(crate) fn on_update_story_album(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_album: telegram_api::StoryAlbum,
    ) {
        let Some(&(owner_dialog_id, story_album_id, _, _)) =
            self.pending_update_story_album_contexts.first()
        else {
            return;
        };
        let album =
            crate::story_album::StoryAlbum::from_telegram(env, owner_dialog_id, story_album);
        if album.get_story_album_id() != story_album_id {
            let status = Status::error(500, "Receive invalid album");
            env.on_get_dialog_error(owner_dialog_id, &status, "UpdateStoryAlbumQuery");
            self.on_update_story_album_error(env, status);
            return;
        }
        let album_object = album.get_story_album_object(env);
        self.pending_update_story_album_contexts.remove(0);
        let mut promise = self.pending_update_story_albums.remove(0);
        promise.set_value(album_object);
    }

    /// `StoryManager::get_can_post_story_result_object` — the nullable
    /// `force = false` overload the CanSendStory/StartLiveStory error
    /// tails read (StoryManager.cpp:1325/:1562): the mapped bands answer
    /// the value, anything else returns `None` and the tail forwards the
    /// error.
    pub(crate) fn try_get_can_post_story_result_object(
        env: &mut impl StoryManagerEnv,
        status: &Status,
    ) -> Option<td_api::CanPostStoryResult> {
        let message = status.message();
        if message == "PREMIUM_ACCOUNT_REQUIRED" {
            return Some(td_api::CanPostStoryResult::canPostStoryResultPremiumNeeded(
                td_api::canPostStoryResultPremiumNeeded,
            ));
        }
        if message == "BOOSTS_REQUIRED" {
            return Some(td_api::CanPostStoryResult::canPostStoryResultBoostNeeded(
                td_api::canPostStoryResultBoostNeeded,
            ));
        }
        if message == "STORIES_TOO_MUCH" {
            return Some(
                td_api::CanPostStoryResult::canPostStoryResultActiveStoryLimitExceeded(
                    td_api::canPostStoryResultActiveStoryLimitExceeded,
                ),
            );
        }
        for (prefix, monthly) in [
            ("STORY_SEND_FLOOD_WEEKLY_", false),
            ("STORY_SEND_FLOOD_MONTHLY_", true),
        ] {
            if let Some(tail) = message.strip_prefix(prefix)
                && let Ok(next_date) = tail.parse::<i32>()
                && next_date > 0
            {
                let retry_after = (next_date - env.unix_time()).max(0);
                if retry_after > 0 {
                    return Some(if monthly {
                        td_api::CanPostStoryResult::canPostStoryResultMonthlyLimitExceeded(
                            td_api::canPostStoryResultMonthlyLimitExceeded { retry_after },
                        )
                    } else {
                        td_api::CanPostStoryResult::canPostStoryResultWeeklyLimitExceeded(
                            td_api::canPostStoryResultWeeklyLimitExceeded { retry_after },
                        )
                    });
                }
                // the window elapsed (StoryManager.cpp:4298-4300) — the
                // `canPostStoryResultOk(1)` value, not an error
                return Some(td_api::CanPostStoryResult::canPostStoryResultOk(
                    td_api::canPostStoryResultOk { story_count: 1 },
                ));
            }
        }
        if let Some(tail) = message.strip_prefix("STORY_LIVE_ALREADY_")
            && let Ok(story_id) = tail.parse::<i32>()
            && StoryId::new(story_id).is_server()
        {
            return Some(
                td_api::CanPostStoryResult::canPostStoryResultLiveStoryIsActive(
                    td_api::canPostStoryResultLiveStoryIsActive { story_id },
                ),
            );
        }
        None
    }

    /// The CanSendStoryQuery answer (StoryManager.cpp:1317-1321) — the
    /// remaining-count value.
    pub(crate) fn on_can_send_story(&mut self, count_remains: i32) {
        let promises = std::mem::take(&mut self.pending_can_post_story);
        for mut promise in promises {
            promise.set_value(td_api::CanPostStoryResult::canPostStoryResultOk(
                td_api::canPostStoryResultOk {
                    story_count: count_remains,
                },
            ));
        }
    }

    /// The CanSendStoryQuery error tail (StoryManager.cpp:1323-1334) —
    /// the mapped bands answer the value, the rest run the head and
    /// forward the error.
    pub(crate) fn on_can_send_story_error(
        &mut self,
        env: &mut impl StoryManagerEnv,
        dialog_id: DialogId,
        status: Status,
    ) {
        if let Some(result) = Self::try_get_can_post_story_result_object(env, &status) {
            let promises = std::mem::take(&mut self.pending_can_post_story);
            for mut promise in promises {
                promise.set_value(result.clone());
            }
            return;
        }
        env.on_get_dialog_error(dialog_id, &status, "CanSendStoryQuery");
        let promises = std::mem::take(&mut self.pending_can_post_story);
        for mut promise in promises {
            promise.set_error(status.clone());
        }
    }

    /// The StartLiveStoryQuery answer (StoryManager.cpp:1532-1558) — the
    /// users/chats land through the intakes, the extracted story through
    /// `on_get_story`, and the ok value carries the story object; the
    /// unexpected-answer bands fail with the C++ 400.
    pub(crate) fn on_start_live_story(
        &mut self,
        env: &mut impl StoryManagerEnv,
        dialog_id: DialogId,
        mut updates: Box<telegram_api::Updates>,
    ) {
        let promise = self.take_start_live_story_promise();
        let Some(mut promise) = promise else {
            return;
        };
        on_updates_users_and_chats(env, &mut updates);
        let story = crate::updates_manager::extract_story(
            &mut updates,
            crate::chat_manager::DialogId::new(dialog_id.get()),
            false,
        );
        let Some(story) = story else {
            tdutils::log_error!("Receive unexpected live story: {updates:?}");
            promise.set_error(Status::error(400, "Failed to create live story"));
            return;
        };
        let story_id = self.on_get_story(env, dialog_id, Some(story));
        if !story_id.is_valid() {
            promise.set_error(Status::error(400, "Failed to create live story"));
            return;
        }
        let story_object = self
            .get_story_object(env, StoryFullId::new(dialog_id, story_id))
            .expect("the story was just registered");
        promise.set_value(td_api::StartLiveStoryResult::startLiveStoryResultOk(
            td_api::startLiveStoryResultOk {
                story: Some(story_object),
            },
        ));
    }

    /// The StartLiveStoryQuery error tail (StoryManager.cpp:1560-1569) —
    /// the mapped bands answer `startLiveStoryResultFail` with the
    /// value, the rest run the head and forward the error.
    pub(crate) fn on_start_live_story_error(
        &mut self,
        env: &mut impl StoryManagerEnv,
        dialog_id: DialogId,
        status: Status,
    ) {
        let promise = self.take_start_live_story_promise();
        let Some(mut promise) = promise else {
            return;
        };
        if let Some(result) = Self::try_get_can_post_story_result_object(env, &status) {
            promise.set_value(td_api::StartLiveStoryResult::startLiveStoryResultFail(
                td_api::startLiveStoryResultFail {
                    error_type: Some(result),
                },
            ));
            return;
        }
        env.on_get_dialog_error(dialog_id, &status, "StartLiveStoryQuery");
        promise.set_error(status);
    }

    /// The parked `startLiveStory` promise pop (FIFO per kind).
    fn take_start_live_story_promise(&mut self) -> Option<Promise<td_api::StartLiveStoryResult>> {
        if self.pending_start_live_story.is_empty() {
            None
        } else {
            Some(self.pending_start_live_story.remove(0))
        }
    }

    /// `StoryManager::on_get_dialogs_to_send_stories`
    /// (StoryManager.cpp:5862-5877).
    pub fn on_get_dialogs_to_send_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        chats: Vec<telegram_api::Chat>,
    ) {
        let channel_ids = env.get_channel_ids(chats, "on_get_dialogs_to_send_stories");
        if self.channels_to_send_stories_inited && self.channels_to_send_stories == channel_ids {
            return;
        }
        self.channels_to_send_stories.clear();
        for channel_id in channel_ids {
            env.force_create_dialog(
                DialogId::from_channel_id(crate::chat_manager::ChannelId::new(channel_id)),
                "on_get_dialogs_to_send_stories",
                false,
            );
            if env.get_channel_status(channel_id).can_post_stories() {
                self.channels_to_send_stories.push(channel_id);
            }
        }
        self.channels_to_send_stories_inited = true;

        self.save_channels_to_send_stories(env);
    }

    /// The GetChatsToSendStoriesQuery answer promise tail
    /// (StoryManager.cpp:1288-1292) — after the intake refreshes the
    /// channel list, every parked promise answers with the chat list.
    pub(crate) fn on_get_dialogs_to_send_stories_result(
        &mut self,
        env: &mut impl StoryManagerEnv,
        chats: Vec<telegram_api::Chat>,
    ) {
        self.on_get_dialogs_to_send_stories(env, chats);
        let channels = self.channels_to_send_stories.clone();
        let promises = std::mem::take(&mut self.pending_dialogs_to_send_stories);
        for promise in promises {
            self.return_dialogs_to_send_stories(env, promise, &channels);
        }
    }

    /// The EditBusinessStoryQuery answer
    /// (`StoryManager::on_edit_business_story`, StoryManager.cpp:6441-6473)
    /// — the extracted story must match the edited id; the ok value
    /// carries the story object and the local row deletes after (the
    /// C++ `on_delete_story` tail).
    pub(crate) fn on_edit_business_story(
        &mut self,
        env: &mut impl StoryManagerEnv,
        send_story_num: u32,
        mut updates: Box<telegram_api::Updates>,
    ) {
        let Some(edited) = self.being_edited_business_stories.remove(&send_story_num) else {
            return;
        };
        let BeingEditedBusinessStory {
            dialog_id,
            story_id,
            mut promise,
            ..
        } = edited;
        on_updates_users_and_chats(env, &mut updates);
        let story = crate::updates_manager::extract_story(
            &mut updates,
            crate::chat_manager::DialogId::new(dialog_id.get()),
            true,
        );
        let Some(story) = story else {
            tdutils::log_error!("Receive unexpected edit story result: {updates:?}");
            promise.set_error(Status::error(400, "Failed to edit story"));
            return;
        };
        let new_story_id = self.on_get_story(env, dialog_id, Some(story));
        // the C++ tail runs `on_delete_story` after both branches
        // (StoryManager.cpp:6466-6472)
        let result = if new_story_id != story_id {
            tdutils::log_error!("Receive unexpected {new_story_id} instead of {story_id}");
            tdutils::status::Result::Err(Status::error(400, "Failed to edit story"))
        } else {
            let story_object = self
                .get_story_object(env, StoryFullId::new(dialog_id, new_story_id))
                .expect("the story was just registered");
            tdutils::status::Result::Ok(Some(story_object))
        };
        promise.set_result(result);
        self.on_delete_story(env, StoryFullId::new(dialog_id, story_id));
    }

    /// The EditBusinessStoryQuery error tail
    /// (StoryManager.cpp:1703-1717) — the FILE_PART re-upload band rides
    /// the FileUploader plane (the `on_send_story_file_parts_missing`
    /// re-fire); the rest run the head, drop the pending story row and
    /// fail the promise (`delete_pending_story`'s bot arm,
    /// StoryManager.cpp:6534-6541 — the `updateStoryPostFailed` outbox
    /// is the non-bot edit arm's, so the row drops without it).
    pub(crate) fn on_edit_business_story_error(
        &mut self,
        env: &mut impl StoryManagerEnv,
        send_story_num: u32,
        status: Status,
    ) {
        let Some(edited) = self.being_edited_business_stories.remove(&send_story_num) else {
            return;
        };
        let BeingEditedBusinessStory {
            dialog_id, promise, ..
        } = edited;
        let mut promise = promise;
        assert!(env.is_bot());
        tdutils::log_info!("Receive error for EditBusinessStoryQuery: {status}");
        if !crate::files::get_missing_file_parts(&status).is_empty() {
            tdutils::log_info!("on_send_story_file_parts_missing — [blocked: FileUploader]");
            return;
        }
        env.on_get_dialog_error(dialog_id, &status, "EditBusinessStoryQuery");
        promise.set_error(status);
    }

    /// `StoryManager::get_story_album_stories`
    /// (StoryManager.cpp:3858-3884).
    pub fn get_story_album_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_album_id: StoryAlbumId,
        offset: i32,
        limit: i32,
        mut promise: Promise<td_api::Stories>,
    ) {
        if limit <= 0 {
            promise.set_error(Status::error(400, "Parameter limit must be positive"));
            return;
        }
        let mut status =
            env.check_dialog_access(owner_dialog_id, false, false, "get_story_album_stories");
        if status.is_error() {
            promise.set_error(status.move_as_error());
            return;
        }
        let status = self.check_story_album_id(story_album_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        if offset < 0 {
            promise.set_error(Status::error(400, "Invalid offset specified"));
            return;
        }
        if !self.can_have_stories(env, owner_dialog_id) {
            promise.set_value(td_api::Stories::default());
            return;
        }

        self.pending_album_stories
            .entry((owner_dialog_id, story_album_id))
            .or_default()
            .push(promise);
        env.send_get_album_stories_query(owner_dialog_id, story_album_id, offset, limit);
    }

    /// `StoryManager::on_get_story_album_stories`
    /// (StoryManager.cpp:3886-3896).
    pub fn on_get_story_album_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_album_id: StoryAlbumId,
        stories: Box<telegram_api::stories_Stories>,
    ) {
        let (total_count, story_ids) =
            self.on_get_stories(env, owner_dialog_id, Vec::new(), stories);
        self.update_story_albums(env, owner_dialog_id, &story_ids, story_album_id, true);
        let story_full_ids: Vec<StoryFullId> = story_ids
            .iter()
            .map(|story_id| StoryFullId::new(owner_dialog_id, *story_id))
            .collect();
        let result = self.get_stories_object(env, total_count, &story_full_ids, &[]);
        if let Some(promises) = self
            .pending_album_stories
            .remove(&(owner_dialog_id, story_album_id))
        {
            for mut promise in promises {
                promise.set_value(result.clone());
            }
        }
    }

    /// `StoryManager::update_story_albums` (StoryManager.cpp:3898-3914).
    pub fn update_story_albums(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_ids: &[StoryId],
        story_album_id: StoryAlbumId,
        is_add: bool,
    ) {
        for story_id in story_ids {
            let story_full_id = StoryFullId::new(owner_dialog_id, *story_id);
            self.get_story_force(env, story_full_id, "update_story_albums");
            let Some(story) = self.stories.get_mut(&story_full_id) else {
                continue;
            };
            if is_add == story.album_ids.contains(&story_album_id) {
                continue;
            }
            if is_add {
                story.album_ids.push(story_album_id);
                story.album_ids.sort();
            } else {
                story
                    .album_ids
                    .retain(|album_id| *album_id != story_album_id);
            }
            let story = self
                .stories
                .get(&story_full_id)
                .expect("just updated")
                .clone();
            self.on_story_changed(env, story_full_id, &story, true, true, false);
        }
    }

    /// `StoryManager::create_story_album` (StoryManager.cpp:3916-3924).
    pub fn create_story_album(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        title: &str,
        story_ids: &[StoryId],
        mut promise: Promise<td_api::StoryAlbum>,
    ) {
        let status = self.can_manage_story_albums(env, owner_dialog_id, "create_story_album");
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        let status = self.check_story_ids(story_ids, true);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        if title.is_empty() {
            promise.set_error(Status::error(400, "Story album name must be non-empty"));
            return;
        }
        // the promise parks like the C++ handler member (the answer
        // settles it with the server album)
        self.pending_create_story_albums.push(promise);
        self.pending_create_story_album_contexts
            .push((owner_dialog_id, story_ids.to_vec()));
        env.send_create_story_album_query(owner_dialog_id, title, story_ids);
    }

    /// `StoryManager::reorder_story_albums` (StoryManager.cpp:3926-3933).
    pub fn reorder_story_albums(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_album_ids: &[StoryAlbumId],
        mut promise: Promise<()>,
    ) {
        let status = self.can_manage_story_albums(env, owner_dialog_id, "reorder_story_albums");
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        for story_album_id in story_album_ids {
            let status = self.check_story_album_id(*story_album_id);
            if status.is_error() {
                promise.set_error(status);
                return;
            }
        }
        env.send_reorder_story_albums_query(owner_dialog_id, story_album_ids);
        self.pending_story_album_unit_queries.push(promise);
    }

    /// `StoryManager::delete_story_album` (StoryManager.cpp:3935-3939).
    pub fn delete_story_album(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_album_id: StoryAlbumId,
        mut promise: Promise<()>,
    ) {
        let status = self.can_manage_story_albums(env, owner_dialog_id, "delete_story_album");
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        let status = self.check_story_album_id(story_album_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        env.send_delete_story_album_query(owner_dialog_id, story_album_id);
        self.pending_story_album_unit_queries.push(promise);
    }

    /// `StoryManager::set_story_album_title` (StoryManager.cpp:3941-3950).
    pub fn set_story_album_title(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_album_id: StoryAlbumId,
        title: &str,
        mut promise: Promise<td_api::StoryAlbum>,
    ) {
        let status = self.can_manage_story_albums(env, owner_dialog_id, "set_story_album_title");
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        let status = self.check_story_album_id(story_album_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        if title.is_empty() {
            promise.set_error(Status::error(400, "Story album name must be non-empty"));
            return;
        }
        // the promise parks like the C++ handler member (the answer
        // settles it with the server album)
        self.pending_update_story_albums.push(promise);
        self.pending_update_story_album_contexts.push((
            owner_dialog_id,
            story_album_id,
            Vec::new(),
            Vec::new(),
        ));
        env.send_update_story_album_query(owner_dialog_id, story_album_id, title, &[], &[], &[]);
    }

    /// `StoryManager::add_story_album_stories` (StoryManager.cpp:3952-3961).
    pub fn add_story_album_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_album_id: StoryAlbumId,
        story_ids: &[StoryId],
        mut promise: Promise<td_api::StoryAlbum>,
    ) {
        let status = self.can_manage_story_albums(env, owner_dialog_id, "add_story_album_stories");
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        let status = self.check_story_album_id(story_album_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        let status = self.check_story_ids(story_ids, true);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        self.update_story_albums(env, owner_dialog_id, story_ids, story_album_id, true);
        // the promise parks like the C++ handler member; the optimistic
        // local `update_story_albums(true)` above rolls back at the
        // error tail (the handler's `deleted_story_ids_` member)
        self.pending_update_story_albums.push(promise);
        self.pending_update_story_album_contexts.push((
            owner_dialog_id,
            story_album_id,
            Vec::new(),
            story_ids.to_vec(),
        ));
        env.send_update_story_album_query(owner_dialog_id, story_album_id, "", &[], story_ids, &[]);
    }

    /// `StoryManager::remove_story_album_stories`
    /// (StoryManager.cpp:3963-3972).
    pub fn remove_story_album_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_album_id: StoryAlbumId,
        story_ids: &[StoryId],
        mut promise: Promise<td_api::StoryAlbum>,
    ) {
        let status =
            self.can_manage_story_albums(env, owner_dialog_id, "remove_story_album_stories");
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        let status = self.check_story_album_id(story_album_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        let status = self.check_story_ids(story_ids, true);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        self.update_story_albums(env, owner_dialog_id, story_ids, story_album_id, false);
        // the promise parks like the C++ handler member; the optimistic
        // local `update_story_albums(false)` above rolls back at the
        // error tail (the handler's `added_story_ids_` member)
        self.pending_update_story_albums.push(promise);
        self.pending_update_story_album_contexts.push((
            owner_dialog_id,
            story_album_id,
            story_ids.to_vec(),
            Vec::new(),
        ));
        env.send_update_story_album_query(owner_dialog_id, story_album_id, "", story_ids, &[], &[]);
    }

    /// `StoryManager::reorder_story_album_stories`
    /// (StoryManager.cpp:3974-3982).
    pub fn reorder_story_album_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_album_id: StoryAlbumId,
        story_ids: &[StoryId],
        mut promise: Promise<td_api::StoryAlbum>,
    ) {
        let status =
            self.can_manage_story_albums(env, owner_dialog_id, "remove_story_album_stories");
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        let status = self.check_story_album_id(story_album_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        let status = self.check_story_ids(story_ids, true);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        // the promise parks like the C++ handler member (the reorder arm
        // carries no rollback members)
        self.pending_update_story_albums.push(promise);
        self.pending_update_story_album_contexts.push((
            owner_dialog_id,
            story_album_id,
            Vec::new(),
            Vec::new(),
        ));
        env.send_update_story_album_query(owner_dialog_id, story_album_id, "", &[], &[], story_ids);
    }

    /// `StoryManager::activate_stealth_mode` (StoryManager.cpp:3984-3986).
    pub fn activate_stealth_mode(&mut self, env: &mut impl StoryManagerEnv, promise: Promise<()>) {
        let _ = env;
        self.pending_stealth_mode.push(promise);
        // `ActivateStealthModeQuery::send` (StoryManager.cpp:1228-1232) —
        // the both-windows form; the answer's Updates carry the stealth
        // mode through the engine, which settles the promise.
        self.send_story_net_query(
            StoryNetKind::ActivateStealthMode,
            telegram_api::Function::stories_activateStealthMode(
                telegram_api::stories_activateStealthMode {
                    flags: 0,
                    past: true,
                    future: true,
                },
            ),
            "stories.activateStealthMode",
        );
    }

    /// `StoryManager::register_story` (StoryManager.cpp:4020-4035) — the
    /// message registration maps are [blocked: MessagesManager track]; the
    /// port keeps the bot guard and the existence note.
    pub fn register_story(
        &mut self,
        env: &impl StoryManagerEnv,
        story_full_id: StoryFullId,
        source: &str,
    ) {
        if env.is_bot() {
            return;
        }
        assert!(story_full_id.is_server());
        let _ = source;
    }

    /// `StoryManager::unregister_story` (StoryManager.cpp:4037-4061) — same
    /// [blocked: MessagesManager track] note.
    pub fn unregister_story(
        &mut self,
        env: &impl StoryManagerEnv,
        story_full_id: StoryFullId,
        source: &str,
    ) {
        if env.is_bot() {
            return;
        }
        assert!(story_full_id.is_server());
        let _ = source;
    }

    /// `StoryManager::register_story_album` (StoryManager.cpp:4078-4098).
    pub fn register_story_album(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_album_full_id: crate::story_ids::StoryAlbumFullId,
        story_album: &crate::story_album::StoryAlbum,
    ) {
        if env.is_bot() {
            return;
        }
        let key = (
            story_album_full_id.get_dialog_id(),
            story_album_full_id.get_story_album_id(),
        );
        self.story_album_file_ids
            .insert(key, story_album.get_file_ids(env));
    }

    /// `StoryManager::get_story_file_source_id` (StoryManager.cpp:5628-5642)
    /// — [blocked: FileSource unification]; the interim map pins existence.
    pub fn get_story_file_source_id(
        &mut self,
        env: &impl StoryManagerEnv,
        story_full_id: StoryFullId,
    ) -> bool {
        if env.is_bot() || !story_full_id.is_server() {
            return false;
        }
        self.story_full_id_to_file_source_id
            .insert(story_full_id, ())
            .is_none()
    }

    /// `StoryManager::get_next_yet_unsent_story_id`
    /// (StoryManager.cpp:5746-5754).
    fn get_next_yet_unsent_story_id(&mut self, dialog_id: DialogId) -> Result<StoryId> {
        let story_id = self
            .current_yet_unsent_story_ids
            .entry(dialog_id)
            .or_default();
        if *story_id == 0 {
            *story_id = StoryId::MAX_SERVER_STORY_ID;
        } else if *story_id == i32::MAX {
            return Result::Err(Status::error(
                400,
                "Tried to send too many stories above daily limit",
            ));
        }
        *story_id += 1;
        Result::Ok(StoryId::new(*story_id))
    }

    /// `return_dialogs_to_send_stories` (StoryManager.cpp:5756-5764).
    fn return_dialogs_to_send_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        promise: Promise<td_api::Chats>,
        channel_ids: &[i64],
    ) {
        let dialog_ids: Vec<DialogId> = channel_ids
            .iter()
            .map(|&id| DialogId::from_channel_id(crate::chat_manager::ChannelId::new(id)))
            .collect();
        let chats = env.get_chats_object(-1, &dialog_ids, "return_dialogs_to_send_stories");
        let mut promise = promise;
        promise.set_value(chats);
    }

    /// `StoryManager::get_dialogs_to_send_stories`
    /// (StoryManager.cpp:5766-5812).
    pub fn get_dialogs_to_send_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        promise: Promise<td_api::Chats>,
    ) {
        if self.channels_to_send_stories_inited {
            let channels = self.channels_to_send_stories.clone();
            self.return_dialogs_to_send_stories(env, promise, &channels);
            return;
        }

        if self.pending_dialogs_to_send_stories.is_empty() && env.use_message_database() {
            let pmc_key = "channels_to_send_stories";
            let value = env.binlog_pmc_get_raw(pmc_key.as_bytes());
            if !value.is_empty() {
                let value = String::from_utf8_lossy(&value).into_owned();
                let mut channel_ids = Vec::new();
                let mut is_bad = false;
                for part in value.split(',') {
                    match part.parse::<i64>() {
                        Ok(id) if crate::chat_manager::ChannelId::new(id).is_valid() => {
                            let channel_id = crate::chat_manager::ChannelId::new(id);
                            env.force_create_dialog(
                                DialogId::from_channel_id(channel_id),
                                "get_dialogs_to_send_stories",
                                true,
                            );
                            channel_ids.push(channel_id.get());
                        }
                        _ => {
                            tdutils::log_error!("Can't parse {value}");
                            is_bad = true;
                            break;
                        }
                    }
                }
                if is_bad {
                    env.binlog_pmc_erase(pmc_key.as_bytes());
                } else {
                    self.channels_to_send_stories = channel_ids
                        .iter()
                        .copied()
                        .filter(|&channel_id| env.get_channel_status(channel_id).can_post_stories())
                        .collect();
                    self.channels_to_send_stories_inited = true;

                    let channels = self.channels_to_send_stories.clone();
                    self.return_dialogs_to_send_stories(env, promise, &channels);
                    return;
                }
            }
        }

        self.reload_dialogs_to_send_stories(env, promise);
    }

    /// `StoryManager::reload_dialogs_to_send_stories`
    /// (StoryManager.cpp:5814-5822).
    pub fn reload_dialogs_to_send_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        promise: Promise<td_api::Chats>,
    ) {
        let is_first = self.pending_dialogs_to_send_stories.is_empty();
        self.pending_dialogs_to_send_stories.push(promise);
        if is_first {
            env.send_get_chats_to_send_stories_query();
        }
    }

    /// `StoryManager::finish_get_dialogs_to_send_stories`
    /// (StoryManager.cpp:5824-5839).
    pub fn finish_get_dialogs_to_send_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        result: Result<()>,
    ) {
        let promises = std::mem::take(&mut self.pending_dialogs_to_send_stories);
        let Result::Err(status) = result else {
            let channels = self.channels_to_send_stories.clone();
            for promise in promises {
                self.return_dialogs_to_send_stories(env, promise, &channels);
            }
            return;
        };
        for mut promise in promises {
            promise.set_error(status.clone());
        }
    }

    /// `StoryManager::save_channels_to_send_stories`
    /// (StoryManager.cpp:5879-5887).
    fn save_channels_to_send_stories(&mut self, env: &mut impl StoryManagerEnv) {
        if env.use_message_database() {
            let value = self
                .channels_to_send_stories
                .iter()
                .map(|channel_id| channel_id.to_string())
                .collect::<Vec<String>>()
                .join(",");
            env.binlog_pmc_set_raw("channels_to_send_stories".as_bytes(), value.as_bytes());
        }
    }

    /// `StoryManager::update_dialogs_to_send_stories`
    /// (StoryManager.cpp:5841-5860).
    pub fn update_dialogs_to_send_stories(
        &mut self,
        env: &mut impl StoryManagerEnv,
        channel_id: i64,
        can_send_stories: bool,
    ) {
        if self.channels_to_send_stories_inited {
            assert!(!env.is_bot());
            let mut was_changed = false;
            if !can_send_stories {
                let old_len = self.channels_to_send_stories.len();
                self.channels_to_send_stories.retain(|&id| id != channel_id);
                was_changed = self.channels_to_send_stories.len() != old_len;
            } else if !self.channels_to_send_stories.contains(&channel_id) {
                self.channels_to_send_stories.push(channel_id);
                was_changed = true;
            }
            if was_changed {
                self.save_channels_to_send_stories(env);
            }
        }
    }

    /// `StoryManager::can_send_story` (StoryManager.cpp:5889-5898).
    pub fn can_send_story(
        &mut self,
        env: &mut impl StoryManagerEnv,
        dialog_id: DialogId,
        mut promise: Promise<td_api::CanPostStoryResult>,
    ) {
        if !env.have_dialog_force(dialog_id, "can_send_story") {
            promise.set_error(Status::error(400, "Chat not found"));
            return;
        }
        if !self.can_post_stories(env, dialog_id) {
            promise.set_error(Status::error(
                400,
                "Not enough rights to post stories in the chat",
            ));
            return;
        }
        self.pending_can_post_story.push(promise);
        env.send_can_send_story_query(dialog_id);
    }

    /// `StoryManager::send_story` (StoryManager.cpp:5900-6006) — the
    /// validations and the local story build are real; the upload/send
    /// machine is the [blocked: FileUploader] seam and the query parks.
    #[allow(clippy::too_many_arguments)]
    pub fn send_story(
        &mut self,
        env: &mut impl StoryManagerEnv,
        dialog_id: DialogId,
        input_story_content: Option<td_api::InputStoryContent>,
        input_areas: Option<td_api::InputStoryAreas>,
        input_caption: Option<td_api::FormattedText>,
        settings: Option<td_api::StoryPrivacySettings>,
        story_album_ids: Vec<StoryAlbumId>,
        active_period: i32,
        from_story_full_id: Option<td_api::StoryFullId>,
        is_pinned: bool,
        protect_content: bool,
        mut promise: Promise<Option<td_api::Story>>,
    ) {
        if !env.have_dialog_force(dialog_id, "send_story") {
            promise.set_error(Status::error(400, "Chat not found"));
            return;
        }
        if !self.can_post_stories(env, dialog_id) {
            promise.set_error(Status::error(
                400,
                "Not enough rights to post stories in the chat",
            ));
            return;
        }
        for story_album_id in &story_album_ids {
            let status = self.check_story_album_id(*story_album_id);
            if status.is_error() {
                promise.set_error(status);
                return;
            }
        }

        let is_bot = env.is_bot();
        if active_period != 86400 && !(active_period == 60 || active_period == 300) {
            let is_premium = env.get_option_boolean("is_premium") || is_bot;
            if !is_premium || ![6 * 3600, 12 * 3600, 2 * 86400].contains(&active_period) {
                promise.set_error(Status::error(400, "Invalid story active period specified"));
                return;
            }
        }

        if is_bot && let Some(from_story_full_id) = from_story_full_id {
            let from_dialog_id = DialogId::new(from_story_full_id.poster_chat_id);
            let from_story_id = StoryId::new(from_story_full_id.story_id);
            if !from_story_id.is_valid()
                || from_dialog_id.get_type() != DialogType::User
                || !env.have_input_peer(from_dialog_id, false, false)
            {
                promise.set_error(Status::error(400, "Invalid story to repost specified"));
                return;
            }
            // `RepostBusinessStoryQuery::send` (StoryManager.cpp:5932-5966)
            // — the `stories.sendStory` repost form: the everyone rules,
            // the empty media, the forward pair. The object promise rides
            // the query (§5 — the interim settled `None` at the send).
            let Some(input_peer) =
                env.get_input_peer(dialog_id, crate::user_manager::AccessRights::Write)
            else {
                let status = Status::error(400, "Can't access the chat");
                env.on_get_dialog_error(dialog_id, &status, "RepostBusinessStoryQuery");
                promise.set_error(status);
                return;
            };
            let fwd_input_peer = env
                .get_input_peer(from_dialog_id, crate::user_manager::AccessRights::Read)
                .expect("the repost gate guaranteed the peer");
            let mut flags = 1 << 6; // FWD_FROM_ID_MASK | FWD_FROM_STORY_MASK (the shared bit)
            if active_period != 86400 {
                flags |= 1 << 3; // PERIOD_MASK
            }
            let everyone_rules = match UserPrivacySettingRules::from_story_privacy_settings(Some(
                td_api::StoryPrivacySettings::storyPrivacySettingsEveryone(
                    td_api::storyPrivacySettingsEveryone::default(),
                ),
            )) {
                Result::Ok(rules) => rules,
                Result::Err(status) => {
                    promise.set_error(status);
                    return;
                }
            };
            let privacy_rules = everyone_rules.get_input_privacy_rules(env);
            self.pending_repost_stories.push(promise);
            self.send_story_net_query(
                StoryNetKind::RepostBusinessStory { dialog_id },
                telegram_api::Function::stories_sendStory(telegram_api::stories_sendStory {
                    flags,
                    pinned: is_pinned,
                    noforwards: protect_content,
                    fwd_modified: false,
                    peer: Some(Box::new(input_peer)),
                    media: Some(Box::new(telegram_api::InputMedia::inputMediaEmpty(
                        telegram_api::inputMediaEmpty {},
                    ))),
                    media_areas: Vec::new(),
                    caption: String::new(),
                    entities: Vec::new(),
                    privacy_rules,
                    random_id: tdutils::random::secure_int64(),
                    period: active_period,
                    fwd_from_id: Some(Box::new(fwd_input_peer)),
                    fwd_from_story: from_story_id.get(),
                    albums: Vec::new(),
                    music: None,
                }),
                "stories.sendStory(repost)",
            );
            return;
        }

        let content = match get_input_story_content(env, input_story_content, dialog_id) {
            Result::Ok(content) => content,
            Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let mut caption = FormattedText::default();
        if let Some(input_caption) = input_caption {
            // the td_api caption-entity intake rides the [blocked:
            // message-text integration]; the interim keeps the text
            let _ = input_caption.entities;
            caption = FormattedText {
                text: input_caption.text,
                entities: Vec::new(),
            };
        }
        let mut settings = settings;
        if dialog_id != env.get_my_dialog_id() && !is_bot {
            settings = Some(td_api::StoryPrivacySettings::storyPrivacySettingsEveryone(
                td_api::storyPrivacySettingsEveryone::default(),
            ));
        }
        let privacy_rules = match UserPrivacySettingRules::from_story_privacy_settings(settings) {
            Result::Ok(rules) => rules,
            Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let mut forward_info = None;
        let mut forward_from_story_full_id = StoryFullId::default();
        if let Some(from_story_full_id) = from_story_full_id {
            forward_from_story_full_id = StoryFullId::new(
                DialogId::new(from_story_full_id.poster_chat_id),
                StoryId::new(from_story_full_id.story_id),
            );
            let Some(story) = self.get_story(forward_from_story_full_id) else {
                promise.set_error(Status::error(400, "Story to repost not found"));
                return;
            };
            if story.content.is_none() {
                promise.set_error(Status::error(400, "Story to repost not found"));
                return;
            }
            if story.noforwards {
                promise.set_error(Status::error(400, "Story can't be reposted"));
                return;
            }
            forward_info = Some(match &story.forward_info {
                Some(story_forward_info) => story_forward_info.clone(),
                None => StoryForwardInfo::from_story_full_id(forward_from_story_full_id, true),
            });
        }
        let story_id = match self.get_next_yet_unsent_story_id(dialog_id) {
            Result::Ok(story_id) => story_id,
            Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let areas = get_media_areas(input_areas, &[]);
        if !is_bot && !env.get_option_boolean("can_use_text_entities_in_story_caption") {
            caption.entities.clear();
        }

        let mut story = Story {
            date: env.unix_time(),
            expire_date: env.unix_time() + active_period,
            is_pinned,
            is_outgoing: true,
            noforwards: protect_content,
            privacy_rules,
            album_ids: story_album_ids,
            content: Some(content),
            forward_info,
            areas,
            caption,
            ..Story::default()
        };
        if dialog_id.get_type() == DialogType::Channel
            && env.is_megagroup_channel(dialog_id.get_channel_id().get())
            && !env.is_anonymous_administrator(dialog_id)
        {
            story.sender_dialog_id = env.get_my_dialog_id();
        }

        let mut random_id;
        loop {
            random_id = tdutils::random::secure_int64();
            if random_id != 0 && !self.being_sent_stories.contains_key(&random_id) {
                break;
            }
        }

        self.send_story_count += 1;
        let send_story_num = self.send_story_count;
        let pending_story = PendingStory {
            dialog_id,
            story_id,
            forward_from_story_full_id,
            send_story_num,
            random_id,
            was_reuploaded: false,
            story: story.clone(),
        };
        self.yet_unsent_stories
            .entry(dialog_id)
            .or_default()
            .insert(send_story_num);
        self.yet_unsent_story_ids
            .entry(dialog_id)
            .or_default()
            .push(story_id);
        self.being_sent_stories
            .insert(random_id, StoryFullId::new(dialog_id, story_id));
        self.being_sent_story_random_ids
            .insert(StoryFullId::new(dialog_id, story_id), random_id);

        // `SendStoryQuery::send` (StoryManager.cpp:1334-1400) — the
        // `delete_pending_story` lambda capture parks as the answer tail
        // (the handler's `pending_story_` member). The query dispatch
        // itself is StoryContent-gated — [blocked: StoryContent → input
        // media] (`get_story_content_input_media` + the uploaded
        // InputFile ride the story-content/Files planes; noted, not
        // silently skipped — net-drain 19 / #28).
        self.pending_send_story_tail.push(pending_story);
        tdutils::log_info!("Send stories.sendStory — [blocked: StoryContent → input media]");

        promise.set_value(self.get_story_object_for(
            env,
            StoryFullId::new(dialog_id, story_id),
            &story,
        ));
        let _ = &mut story;
    }

    /// `StoryManager::delete_pending_story` (StoryManager.cpp:6521-6605) —
    /// the not-sent collapse: the parked story rows go away and the update
    /// fires like the C++ failure path.
    pub fn delete_pending_story(
        &mut self,
        env: &mut impl StoryManagerEnv,
        pending_story: PendingStory,
        status: Status,
    ) {
        let story_full_id = StoryFullId::new(pending_story.dialog_id, pending_story.story_id);
        if let Some(story) = self.stories.remove(&story_full_id) {
            let story_object = self.get_story_object_for(env, story_full_id, &story);
            env.send_update(td_api::Update::updateStoryPostFailed(
                td_api::updateStoryPostFailed {
                    story: story_object,
                    error: Some(td_api::Error {
                        code: status.code(),
                        message: status.message().to_owned(),
                    }),
                    error_type: Some(Self::get_can_post_story_result_object(&status, true)),
                },
            ));
        }
        if let Some(story_ids) = self.yet_unsent_story_ids.get_mut(&pending_story.dialog_id) {
            story_ids.retain(|story_id| *story_id != pending_story.story_id);
        }
        self.being_sent_stories.remove(&pending_story.random_id);
        self.being_sent_story_random_ids.remove(&story_full_id);
        let active_stories = self.active_stories.get(&pending_story.dialog_id).cloned();
        self.send_update_chat_active_stories(
            env,
            pending_story.dialog_id,
            active_stories.as_ref(),
            "delete_pending_story",
        );
    }

    /// `StoryManager::start_live_story` (StoryManager.cpp:6207-6229).
    #[allow(clippy::too_many_arguments)]
    pub fn start_live_story(
        &mut self,
        env: &mut impl StoryManagerEnv,
        dialog_id: DialogId,
        mut settings: Option<td_api::StoryPrivacySettings>,
        is_pinned: bool,
        protect_content: bool,
        is_rtmp_stream: bool,
        enable_messages: bool,
        paid_message_star_count: i64,
        mut promise: Promise<td_api::StartLiveStoryResult>,
    ) {
        if !env.have_dialog_force(dialog_id, "start_live_story") {
            promise.set_error(Status::error(400, "Chat not found"));
            return;
        }
        if !self.can_post_stories(env, dialog_id) {
            promise.set_error(Status::error(
                400,
                "Not enough rights to post stories in the chat",
            ));
            return;
        }
        if env.is_group_dialog(dialog_id) {
            promise.set_error(Status::error(400, "Can't start live story in the chat"));
            return;
        }
        if dialog_id != env.get_my_dialog_id() {
            settings = Some(td_api::StoryPrivacySettings::storyPrivacySettingsEveryone(
                td_api::storyPrivacySettingsEveryone::default(),
            ));
        }
        let privacy_rules = match UserPrivacySettingRules::from_story_privacy_settings(settings) {
            Result::Ok(rules) => rules,
            Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };

        self.pending_start_live_story.push(promise);
        env.send_start_live_story_query(
            dialog_id,
            &privacy_rules,
            is_pinned,
            protect_content,
            is_rtmp_stream,
            enable_messages,
            paid_message_star_count,
        );
    }

    /// `StoryManager::edit_story` (StoryManager.cpp:6291-6396).
    /// The C++-faithful argument list (the convention's
    /// too_many_arguments note).
    #[allow(clippy::too_many_arguments)]
    pub fn edit_story(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_id: StoryId,
        input_story_content: Option<td_api::InputStoryContent>,
        input_areas: Option<td_api::InputStoryAreas>,
        input_caption: Option<td_api::FormattedText>,
        mut promise: Promise<()>,
    ) {
        assert!(!env.is_bot());
        let story_full_id = StoryFullId::new(owner_dialog_id, story_id);
        let story = self.get_story(story_full_id).cloned();
        let Some(story) = story else {
            promise.set_error(Status::error(400, "Story not found"));
            return;
        };
        if story.content.is_none() {
            promise.set_error(Status::error(400, "Story not found"));
            return;
        }
        if !self.can_edit_story(env, story_full_id, Some(&story)) {
            promise.set_error(Status::error(400, "Story can't be edited"));
            return;
        }

        let mut content = None;
        let mut are_media_areas_edited = input_areas.is_some();
        let areas;
        let mut is_caption_edited = input_caption.is_some();
        let mut caption = FormattedText::default();
        if let Some(input_story_content) = input_story_content {
            match get_input_story_content(env, Some(input_story_content), owner_dialog_id) {
                Result::Ok(parsed) => content = Some(parsed),
                Result::Err(status) => {
                    promise.set_error(status);
                    return;
                }
            }
        }
        if are_media_areas_edited {
            areas = get_media_areas(input_areas, &story.areas);
            let current_areas = self
                .being_edited_stories
                .get(&story_full_id)
                .filter(|edited| edited.edit_media_areas)
                .map(|edited| &edited.areas)
                .unwrap_or(&story.areas);
            if *current_areas == areas {
                are_media_areas_edited = false;
            } else if content.is_none() {
                promise.set_error(Status::error(400, "Can't edit story areas without content"));
                return;
            }
        } else {
            areas = Vec::new();
        }
        if is_caption_edited {
            if let Some(input_caption) = input_caption {
                // [blocked: message-text integration] — see send_story
                let _ = input_caption.entities;
                caption = FormattedText {
                    text: input_caption.text,
                    entities: Vec::new(),
                };
            }
            if !env.get_option_boolean("can_use_text_entities_in_story_caption") {
                caption.entities.clear();
            }
            let current_caption = self
                .being_edited_stories
                .get(&story_full_id)
                .filter(|edited| edited.edit_caption)
                .map(|edited| &edited.caption)
                .unwrap_or(&story.caption);
            if *current_caption == caption {
                is_caption_edited = false;
            }
        }
        if content.is_none() && !are_media_areas_edited && !is_caption_edited {
            promise.set_value(());
            return;
        }

        {
            let edited_story = self.being_edited_stories.entry(story_full_id).or_default();
            if let Some(content) = content {
                edited_story.content = Some(content);
            }
            if are_media_areas_edited {
                edited_story.areas = areas;
                edited_story.edit_media_areas = true;
            }
            if is_caption_edited {
                edited_story.caption = caption;
                edited_story.edit_caption = true;
            }
            edited_story.promises.push(promise);
        }
        *self.edit_generations.entry(story_full_id).or_default() += 1;
        let generation = *self
            .edit_generations
            .get(&story_full_id)
            .expect("just bumped");

        // `EditStoryQuery::send` (StoryManager.cpp:1571-1613) — the
        // Write peer with the edit flags. The caption-only edit
        // dispatches; the content leg needs [blocked: StoryContent →
        // input media] and the areas leg [blocked: MediaArea track →
        // wire areas] — the plane-gated legs are noted, not silently
        // skipped (net-drain 19 / #28).
        let (has_content, edits_media_areas, caption_flags, caption_text) = {
            let edited = self
                .being_edited_stories
                .get(&story_full_id)
                .expect("parked above");
            (
                edited.content.is_some(),
                edited.edit_media_areas,
                edited.edit_caption,
                edited.caption.text.clone(),
            )
        };
        if has_content {
            tdutils::log_info!(
                "Send stories.editStory(media) — [blocked: StoryContent → input media]"
            );
        } else if edits_media_areas {
            tdutils::log_info!("Send stories.editStory(areas) — [blocked: MediaArea track]");
        } else {
            // `EditStoryQuery::send`'s null-peer band is the C++ on_error
            // head + `delete_pending_story`'s edit arm (the parked
            // promises fail through `on_edit_story`).
            match env.get_input_peer(owner_dialog_id, crate::user_manager::AccessRights::Write) {
                Some(input_peer) => {
                    let mut flags = 0;
                    if caption_flags {
                        flags |= 1 << 1; // telegram_api::stories_editStory::CAPTION_MASK (== ENTITIES_MASK)
                    }
                    self.send_story_net_query(
                        StoryNetKind::EditStory {
                            story_full_id,
                            generation,
                        },
                        telegram_api::Function::stories_editStory(
                            telegram_api::stories_editStory {
                                flags,
                                peer: Some(Box::new(input_peer)),
                                id: story_id.get(),
                                media: None,
                                media_areas: Vec::new(),
                                caption: caption_text,
                                entities: Vec::new(),
                                privacy_rules: Vec::new(),
                                music: None,
                            },
                        ),
                        "stories.editStory",
                    );
                }
                None => {
                    let status = Status::error(400, "Can't access the chat");
                    env.on_get_dialog_error(owner_dialog_id, &status, "EditStoryQuery");
                    self.on_edit_story(env, story_full_id, Result::from_status(status));
                    return;
                }
            }
        }

        let story = self.stories.get(&story_full_id).expect("checked").clone();
        self.on_story_changed(env, story_full_id, &story, true, true, false);
    }

    /// `StoryManager::finish_edit_story` — the EditStoryQuery answer: the
    /// parked edit promises settle like the C++
    /// `delete_pending_story`/success path.
    pub fn on_edit_story(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
        result: Result<()>,
    ) {
        let Some(edited_story) = self.being_edited_stories.remove(&story_full_id) else {
            return;
        };
        let promises = edited_story.promises;
        if let Some(story) = self.stories.get(&story_full_id).cloned() {
            self.on_story_changed(env, story_full_id, &story, true, true, false);
        }
        for mut promise in promises {
            promise.set_result(result.clone());
        }
    }

    /// `++send_story_count_` with the u32-wrap skip
    /// (StoryManager.cpp:6417-6419).
    fn next_send_story_num(&mut self) -> u32 {
        self.send_story_count += 1;
        if self.send_story_count == 0 {
            self.send_story_count += 1;
        }
        self.send_story_count
    }

    /// `StoryManager::edit_business_story` (StoryManager.cpp:6398-6439).
    /// The C++-faithful argument list (the convention's
    /// too_many_arguments note).
    #[allow(clippy::too_many_arguments)]
    pub fn edit_business_story(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_id: StoryId,
        input_story_content: Option<td_api::InputStoryContent>,
        input_areas: Option<td_api::InputStoryAreas>,
        input_caption: Option<td_api::FormattedText>,
        settings: Option<td_api::StoryPrivacySettings>,
        mut promise: Promise<Option<td_api::Story>>,
    ) {
        assert!(env.is_bot());
        let story_full_id = StoryFullId::new(owner_dialog_id, story_id);
        if !self.can_edit_story(env, story_full_id, None) {
            promise.set_error(Status::error(400, "Story can't be edited"));
            return;
        }

        let content = match get_input_story_content(env, input_story_content, owner_dialog_id) {
            Result::Ok(content) => content,
            Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let areas = get_media_areas(input_areas, &[]);
        let caption = match input_caption {
            Some(input_caption) => {
                // [blocked: message-text integration] — see send_story
                let _ = input_caption.entities;
                FormattedText {
                    text: input_caption.text,
                    entities: Vec::new(),
                }
            }
            None => FormattedText::default(),
        };
        let privacy_rules = match UserPrivacySettingRules::from_story_privacy_settings(settings) {
            Result::Ok(rules) => rules,
            Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };

        // the C++ parks `BeingEditedBusinessStory` keyed by the send
        // story num, then rides the upload machine
        // (`do_send_story`/`do_edit_story` — [blocked: FileUploader]);
        // the upload leg collapses here onto the direct send when the
        // content's file is already remote, and parks otherwise
        let edit_story_num = self.next_send_story_num();
        self.being_edited_business_stories.insert(
            edit_story_num,
            BeingEditedBusinessStory {
                dialog_id: owner_dialog_id,
                story_id,
                content: content.clone(),
                areas: areas.clone(),
                caption: caption.clone(),
                privacy_rules: privacy_rules.clone(),
                promise,
            },
        );
        match env.get_story_content_input_media(&content) {
            Some(input_media) => env.send_edit_business_story_query(
                owner_dialog_id,
                story_id,
                input_media,
                &areas,
                &caption,
                &privacy_rules,
                edit_story_num,
            ),
            None => tdutils::log_info!(
                "EditBusinessStoryQuery — the content file has no remote location; the upload rides the FileUploader plane"
            ),
        }
    }

    /// `StoryManager::edit_story_cover` (StoryManager.cpp:6491-6519).
    pub fn edit_story_cover(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_id: StoryId,
        main_frame_timestamp: f64,
        mut promise: Promise<()>,
    ) {
        let story_full_id = StoryFullId::new(owner_dialog_id, story_id);
        let story = self.get_story(story_full_id).cloned();
        let Some(story) = story else {
            promise.set_error(Status::error(400, "Story not found"));
            return;
        };
        if story.content.is_none() {
            promise.set_error(Status::error(400, "Story not found"));
            return;
        }
        if !self.can_edit_story(env, story_full_id, Some(&story)) {
            promise.set_error(Status::error(400, "Story can't be edited"));
            return;
        }
        if self.being_edited_stories.contains_key(&story_full_id) {
            promise.set_error(Status::error(400, "Story is being edited"));
            return;
        }
        if main_frame_timestamp < 0.0 {
            promise.set_error(Status::error(400, "Wrong cover timestamp specified"));
            return;
        }
        if story.content.as_ref().expect("checked").get_type() != StoryContentType::Video {
            promise.set_error(Status::error(
                400,
                "Cover timestamp can't be edited for the story",
            ));
            return;
        }

        // `EditStoryCoverQuery` is [blocked: FileUploader + StoryContent
        // → input media] (the cover re-upload); the promise parks
        env.send_edit_story_cover_query(story_full_id, main_frame_timestamp);
        self.pending_unit_queries.push(promise);
    }

    /// `StoryManager::set_story_privacy_settings`
    /// (StoryManager.cpp:6607-6623).
    pub fn set_story_privacy_settings(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_id: StoryId,
        settings: Option<td_api::StoryPrivacySettings>,
        mut promise: Promise<()>,
    ) {
        let owner_dialog_id = env.get_my_dialog_id();
        let story_full_id = StoryFullId::new(owner_dialog_id, story_id);
        let story = self.get_story(story_full_id).cloned();
        let Some(story) = story else {
            promise.set_error(Status::error(400, "Story not found"));
            return;
        };
        if story.content.is_none() {
            promise.set_error(Status::error(400, "Story not found"));
            return;
        }
        if !self.can_set_story_privacy_settings(env, story_full_id, &story) {
            promise.set_error(Status::error(
                400,
                "Story privacy settings can't be changed",
            ));
            return;
        }
        let privacy_rules = match UserPrivacySettingRules::from_story_privacy_settings(settings) {
            Result::Ok(rules) => rules,
            Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        // `EditStoryPrivacyQuery::send` (StoryManager.cpp:613-629) — the
        // Write peer with the privacy rules (the other editStory fields
        // are the C++ empty forms); the null band is the C++ on_error
        // head + the promise error.
        let Some(input_peer) =
            env.get_input_peer(owner_dialog_id, crate::user_manager::AccessRights::Write)
        else {
            let status = Status::error(400, "Can't access the chat");
            env.on_get_dialog_error(owner_dialog_id, &status, "EditStoryPrivacyQuery");
            promise.set_error(status);
            return;
        };
        let rules = privacy_rules.get_input_privacy_rules(env);
        self.pending_edit_story_privacy.push(promise);
        self.send_story_net_query(
            StoryNetKind::EditStoryPrivacy {
                dialog_id: owner_dialog_id,
            },
            telegram_api::Function::stories_editStory(telegram_api::stories_editStory {
                flags: 1 << 2, // telegram_api::stories_editStory::PRIVACY_RULES_MASK
                peer: Some(Box::new(input_peer)),
                id: story_id.get(),
                media: None,
                media_areas: Vec::new(),
                caption: String::new(),
                entities: Vec::new(),
                privacy_rules: rules,
                music: None,
            }),
            "stories.editStory(privacy)",
        );
    }

    /// `StoryManager::toggle_story_is_pinned`
    /// (StoryManager.cpp:6625-6643).
    pub fn toggle_story_is_pinned(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_id: StoryId,
        is_pinned: bool,
        mut promise: Promise<()>,
    ) {
        let story_full_id = StoryFullId::new(owner_dialog_id, story_id);
        let story = self.get_story(story_full_id).cloned();
        let Some(story) = story else {
            promise.set_error(Status::error(400, "Story not found"));
            return;
        };
        if story.content.is_none() {
            promise.set_error(Status::error(400, "Story not found"));
            return;
        }
        if !self.can_toggle_story_is_pinned(env, story_full_id) {
            promise.set_error(Status::error(400, "Story can't be pinned/unpinned"));
            return;
        }
        // `ToggleStoryPinnedQuery::send` (StoryManager.cpp:664-671) — the
        // Write peer with the single id; the null band is the C++
        // on_error head + the promise error (the lambda's error arm).
        let Some(input_peer) =
            env.get_input_peer(owner_dialog_id, crate::user_manager::AccessRights::Write)
        else {
            let status = Status::error(400, "Can't access the chat");
            env.on_get_dialog_error(owner_dialog_id, &status, "ToggleStoryPinnedQuery");
            promise.set_error(status);
            return;
        };
        self.pending_toggle_story_pinned.push(promise);
        self.send_story_net_query(
            StoryNetKind::ToggleStoryPinned {
                story_full_id,
                is_pinned,
            },
            telegram_api::Function::stories_togglePinned(telegram_api::stories_togglePinned {
                peer: Some(Box::new(input_peer)),
                id: vec![story_id.get()],
                pinned: is_pinned,
            }),
            "stories.togglePinned",
        );
    }

    /// `StoryManager::on_toggle_story_is_pinned`
    /// (StoryManager.cpp:6645-6654).
    pub fn on_toggle_story_is_pinned(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
        is_pinned: bool,
    ) {
        if let Some(story) = self.stories.get_mut(&story_full_id) {
            story.is_pinned = is_pinned;
            let story = story.clone();
            self.on_story_changed(env, story_full_id, &story, true, true, false);
        }
    }

    /// `StoryManager::delete_story` (StoryManager.cpp:6656-6687).
    pub fn delete_story(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_id: StoryId,
        mut promise: Promise<()>,
    ) {
        let story_full_id = StoryFullId::new(owner_dialog_id, story_id);
        let story = self.get_story(story_full_id).cloned();
        let Some(story) = story else {
            promise.set_error(Status::error(400, "Story not found"));
            return;
        };
        if !self.can_delete_story(env, story_full_id, &story) {
            promise.set_error(Status::error(400, "Story can't be deleted"));
            return;
        }
        if !story_id.is_server() {
            // the cancel-uploading path rides the [blocked: FileUploader]
            // machine; the interim story is not cancelable
            promise.set_error(Status::error(
                400,
                "Story upload has been already completed",
            ));
            return;
        }

        self.delete_story_on_server(env, story_full_id, promise);
    }

    /// `StoryManager::delete_story_on_server` (StoryManager.cpp:6710-6727).
    pub fn delete_story_on_server(
        &mut self,
        env: &mut impl StoryManagerEnv,
        story_full_id: StoryFullId,
        mut promise: Promise<()>,
    ) {
        tdutils::log_info!("Delete {story_full_id} from server");
        assert!(story_full_id.is_server());

        self.deleted_story_full_ids.insert(story_full_id);

        // `DeleteStoriesQuery::send` (StoryManager.cpp:697-705) — the
        // Write peer with the single id; the null band is the C++
        // on_error head + the promise error. The promise rides the
        // query (the erase-log-event collapse — the binlog id is the
        // blocked zero) and settles at the answer.
        let owner_dialog_id = story_full_id.get_dialog_id();
        let Some(input_peer) =
            env.get_input_peer(owner_dialog_id, crate::user_manager::AccessRights::Write)
        else {
            let status = Status::error(400, "Can't access the chat");
            env.on_get_dialog_error(owner_dialog_id, &status, "DeleteStoriesQuery");
            promise.set_error(status);
            return;
        };
        self.pending_delete_stories.push(promise);
        self.send_story_net_query(
            StoryNetKind::DeleteStories {
                dialog_id: owner_dialog_id,
            },
            telegram_api::Function::stories_deleteStories(telegram_api::stories_deleteStories {
                peer: Some(Box::new(input_peer)),
                id: vec![story_full_id.get_story_id().get()],
            }),
            "stories.deleteStories",
        );

        self.on_delete_story(env, story_full_id);
    }

    /// `StoryManager::remove_story_notifications_by_story_ids`
    /// (StoryManager.cpp:6739-6756).
    pub fn remove_story_notifications_by_story_ids(
        &mut self,
        env: &mut impl StoryManagerEnv,
        dialog_id: DialogId,
        story_ids: &[StoryId],
    ) {
        for story_id in story_ids {
            if !story_id.is_server() {
                tdutils::log_error!("Tried to delete {story_id}");
                continue;
            }
            let story_full_id = StoryFullId::new(dialog_id, *story_id);
            if !self.have_story_force(env, story_full_id) {
                tdutils::log_info!("Can't delete {story_full_id} because it is not found");
                continue;
            }
            self.on_delete_story(env, story_full_id);
        }
    }

    /// `StoryManager::on_update_story_chosen_reaction_type`
    /// (StoryManager.cpp:5374-5396).
    pub fn on_update_story_chosen_reaction_type(
        &mut self,
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_id: StoryId,
        chosen_reaction_type: ReactionType,
    ) {
        if !owner_dialog_id.is_valid() || !story_id.is_server() {
            tdutils::log_error!("Receive chosen reaction in {story_id}");
            return;
        }
        if !env.have_dialog_force(owner_dialog_id, "on_update_story_chosen_reaction_type") {
            return;
        }
        if chosen_reaction_type.is_paid_reaction() {
            tdutils::log_error!("Receive paid reaction for {story_id}");
            return;
        }
        let story_full_id = StoryFullId::new(owner_dialog_id, story_id);
        if self.being_set_story_reactions.contains_key(&story_full_id) {
            tdutils::log_info!(
                "Postpone {story_full_id} chosen reaction update, because there is a pending reaction"
            );
            *self
                .being_set_story_reactions
                .entry(story_full_id)
                .or_default() |= 1;
            return;
        }
        self.get_story_force(env, story_full_id, "on_update_story_chosen_reaction_type");
        self.on_story_chosen_reaction_changed(env, story_full_id, chosen_reaction_type);
    }

    /// `StoryManager::get_current_state` (StoryManager.cpp:6758-6774).
    pub fn get_current_state(
        &self,
        env: &mut impl StoryManagerEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        for (dialog_id, active_stories) in &self.active_stories {
            if self.updated_active_stories.contains(dialog_id) {
                updates.push(self.get_update_chat_active_stories_object(
                    env,
                    *dialog_id,
                    Some(active_stories),
                ));
            }
        }
        if !env.is_bot() {
            for story_list_id in [StoryListId::Main, StoryListId::Archive] {
                let story_list = self.get_story_list(story_list_id);
                if story_list.sent_total_count != -1 {
                    updates.push(
                        self.get_update_story_list_chat_count_object(story_list_id, story_list),
                    );
                }
            }

            updates.push(self.get_update_story_stealth_mode());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_manager_has_no_stories() {
        let manager = StoryManager::new();
        let full_id = StoryFullId::new(
            DialogId::from_user_id(crate::user_manager::UserId::new(42)),
            StoryId::new(1),
        );
        assert!(!manager.have_story(full_id));
        assert!(!manager.is_inaccessible_story(full_id));
    }

    #[test]
    fn can_post_story_result_object_collapses_the_status_family() {
        // StoryManager.cpp:4279-4321 — the Status → CanPostStoryResult
        // collapse.
        assert!(matches!(
            StoryManager::get_can_post_story_result_object(&Status::error(400, "other"), false),
            td_api::CanPostStoryResult::canPostStoryResultOk(_)
        ));
        assert!(matches!(
            StoryManager::get_can_post_story_result_object(&Status::error(429, ""), false),
            td_api::CanPostStoryResult::canPostStoryResultActiveStoryLimitExceeded(_)
        ));
        assert!(matches!(
            StoryManager::get_can_post_story_result_object(
                &Status::error(400, "STORY_SEND_WEEKLY_LIMIT_EXCEEDED"),
                false
            ),
            td_api::CanPostStoryResult::canPostStoryResultWeeklyLimitExceeded(_)
        ));
        assert!(matches!(
            StoryManager::get_can_post_story_result_object(
                &Status::error(400, "STORY_SEND_MONTHLY_LIMIT_EXCEEDED"),
                false
            ),
            td_api::CanPostStoryResult::canPostStoryResultMonthlyLimitExceeded(_)
        ));
        assert!(matches!(
            StoryManager::get_can_post_story_result_object(
                &Status::error(400, "PREMIUM_ACCOUNT_REQUIRED"),
                false
            ),
            td_api::CanPostStoryResult::canPostStoryResultPremiumNeeded(_)
        ));
        assert!(matches!(
            StoryManager::get_can_post_story_result_object(
                &Status::error(400, "BOOST_REQUIRED"),
                false
            ),
            td_api::CanPostStoryResult::canPostStoryResultBoostNeeded(_)
        ));
    }

    #[test]
    fn story_list_new_matches_the_cpp_defaults() {
        // StoryList.h — the -1 sentinel counts.
        let list = StoryList::new();
        assert_eq!(list.server_total_count, -1);
        assert_eq!(list.sent_total_count, -1);
        assert!(list.state.is_empty());
    }
}
