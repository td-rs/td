//! `DialogFilterManager` — the M.filter slice.
//!
//! Port of `td/telegram/DialogFilterManager.{h,cpp}`, `DialogFilter.{h,cpp}`,
//! `DialogFilterId.h` and `DialogFilterInviteLink.{h,cpp}`.
//!
//! Manages chat folders (dialog filters), folder invite links, tags, and
//! emits `updateChatFolders`.

use std::collections::HashSet;

use crate::dialog_id::DialogId;
use crate::td_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// `DialogFilterId` (DialogFilterId.h:19-61).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DialogFilterId(i32);

impl DialogFilterId {
    pub const fn new(id: i32) -> Self {
        Self(id)
    }

    pub const fn min() -> Self {
        Self(2)
    }

    pub const fn max() -> Self {
        Self(255)
    }

    pub fn is_valid(self) -> bool {
        self.0 >= Self::min().0
    }

    pub fn get(self) -> i32 {
        self.0
    }
}

/// Known icon name to emoji mapping (DialogFilter.cpp:902-916).
pub const ICON_NAMES_AND_EMOJIS: &[(&str, &str)] = &[
    ("All", "\u{1F4AC}"),
    ("Unread", "\u{2705}"),
    ("Unmuted", "\u{1F514}"),
    ("Bots", "\u{1F916}"),
    ("Channels", "\u{1F4E2}"),
    ("Groups", "\u{1F465}"),
    ("Private", "\u{1F464}"),
    ("Custom", "\u{1F4C1}"),
    ("Setup", "\u{1F4CB}"),
    ("Cat", "\u{1F431}"),
    ("Crown", "\u{1F451}"),
    ("Favorite", "\u{2B50}"),
    ("Flower", "\u{1F339}"),
    ("Game", "\u{1F3AE}"),
    ("Home", "\u{1F3E0}"),
    ("Love", "\u{2764}"),
    ("Mask", "\u{1F3AD}"),
    ("Party", "\u{1F378}"),
    ("Sport", "\u{26BD}"),
    ("Study", "\u{1F393}"),
    ("Trade", "\u{1F4C8}"),
    ("Travel", "\u{2708}"),
    ("Work", "\u{1F4BC}"),
    ("Airplane", "\u{1F6EB}"),
    ("Book", "\u{1F4D5}"),
    ("Light", "\u{1F4A1}"),
    ("Like", "\u{1F44D}"),
    ("Money", "\u{1F4B0}"),
    ("Note", "\u{1F3B5}"),
    ("Palette", "\u{1F3A8}"),
];

/// `DialogFilter` (DialogFilter.h:29-100).
#[derive(Clone, Debug, PartialEq)]
pub struct DialogFilter {
    pub dialog_filter_id: DialogFilterId,
    pub title: String,
    pub animate_title: bool,
    pub icon_name: String,
    pub emoji: String,
    pub color_id: i32,
    pub is_shareable: bool,
    pub has_my_invites: bool,
    pub pinned_dialog_ids: Vec<DialogId>,
    pub included_dialog_ids: Vec<DialogId>,
    pub excluded_dialog_ids: Vec<DialogId>,
    pub exclude_muted: bool,
    pub exclude_read: bool,
    pub exclude_archived: bool,
    pub include_contacts: bool,
    pub include_non_contacts: bool,
    pub include_bots: bool,
    pub include_groups: bool,
    pub include_channels: bool,
}

impl DialogFilter {
    pub fn is_valid_color_id(color_id: i32) -> bool {
        (-1..=6).contains(&color_id)
    }

    pub fn get_emoji_by_icon_name(icon_name: &str) -> String {
        for &(name, emoji) in ICON_NAMES_AND_EMOJIS {
            if name.eq_ignore_ascii_case(icon_name) {
                return emoji.to_string();
            }
        }
        String::new()
    }

    pub fn get_icon_name_by_emoji(emoji: &str) -> String {
        for &(name, em) in ICON_NAMES_AND_EMOJIS {
            if em == emoji {
                return name.to_string();
            }
        }
        String::new()
    }

    /// `DialogFilter::get_default_icon_name` (DialogFilter.cpp:402-438).
    pub fn get_default_icon_name(filter: &td_api::ChatFolder) -> String {
        if let Some(ref icon) = filter.icon {
            if !icon.name.is_empty() && !Self::get_emoji_by_icon_name(&icon.name).is_empty() {
                return icon.name.clone();
            }
        }

        if !filter.pinned_chat_ids.is_empty()
            || !filter.included_chat_ids.is_empty()
            || !filter.excluded_chat_ids.is_empty()
        {
            return "Custom".to_string();
        }

        if filter.include_contacts || filter.include_non_contacts {
            if !filter.include_bots && !filter.include_groups && !filter.include_channels {
                return "Private".to_string();
            }
        } else {
            if !filter.include_bots && !filter.include_channels {
                if !filter.include_groups {
                    return "Custom".to_string();
                }
                return "Groups".to_string();
            }
            if !filter.include_bots && !filter.include_groups {
                return "Channels".to_string();
            }
            if !filter.include_groups && !filter.include_channels {
                return "Bots".to_string();
            }
        }
        if filter.exclude_read && !filter.exclude_muted {
            return "Unread".to_string();
        }
        if filter.exclude_muted && !filter.exclude_read {
            return "Unmuted".to_string();
        }
        "Custom".to_string()
    }

    pub fn get_chosen_or_default_icon_name(&self) -> String {
        let icon_name = self.get_icon_name();
        if !icon_name.is_empty() {
            return icon_name;
        }

        if !self.pinned_dialog_ids.is_empty()
            || !self.included_dialog_ids.is_empty()
            || !self.excluded_dialog_ids.is_empty()
        {
            return "Custom".to_string();
        }

        if self.include_contacts || self.include_non_contacts {
            if !self.include_bots && !self.include_groups && !self.include_channels {
                return "Private".to_string();
            }
        } else {
            if !self.include_bots && !self.include_channels {
                if !self.include_groups {
                    return "Custom".to_string();
                }
                return "Groups".to_string();
            }
            if !self.include_bots && !self.include_groups {
                return "Channels".to_string();
            }
            if !self.include_groups && !self.include_channels {
                return "Bots".to_string();
            }
        }
        if self.exclude_read && !self.exclude_muted {
            return "Unread".to_string();
        }
        if self.exclude_muted && !self.exclude_read {
            return "Unmuted".to_string();
        }
        "Custom".to_string()
    }

    pub fn get_icon_name(&self) -> String {
        if !self.icon_name.is_empty() {
            return self.icon_name.clone();
        }
        Self::get_icon_name_by_emoji(&self.emoji)
    }

    /// `DialogFilter::create_dialog_filter` (DialogFilter.cpp:111-191).
    pub fn create(
        dialog_filter_id: DialogFilterId,
        filter: td_api::ChatFolder,
        max_dialogs: usize,
    ) -> Result<Self, Status> {
        let name_obj = match filter.name {
            Some(n) => n,
            None => return Err(Status::error(400, "Chat folder name must be non-empty")),
        };
        let text_obj = match name_obj.text {
            Some(t) => t,
            None => return Err(Status::error(400, "Chat folder name must be non-empty")),
        };
        let mut title = text_obj.text.trim().to_string();
        if title.is_empty() {
            return Err(Status::error(400, "Name must be non-empty"));
        }
        // Limit title length to 12 chars
        if title.chars().count() > 12 {
            title = title.chars().take(12).collect();
        }

        let icon_name = filter.icon.map_or(String::new(), |i| i.name);
        let emoji = if !icon_name.is_empty() {
            let em = Self::get_emoji_by_icon_name(&icon_name);
            if em.is_empty() {
                return Err(Status::error(400, "Invalid icon name specified"));
            }
            em
        } else {
            String::new()
        };

        if !Self::is_valid_color_id(filter.color_id) {
            return Err(Status::error(400, "Invalid color identifier specified"));
        }

        let mut seen = HashSet::new();
        let mut pinned_dialog_ids = Vec::new();
        for id in filter.pinned_chat_ids {
            if id != 0 && seen.insert(id) {
                pinned_dialog_ids.push(DialogId::new(id));
            }
        }
        let mut included_dialog_ids = Vec::new();
        for id in filter.included_chat_ids {
            if id != 0 && seen.insert(id) {
                included_dialog_ids.push(DialogId::new(id));
            }
        }
        let mut excluded_dialog_ids = Vec::new();
        for id in filter.excluded_chat_ids {
            if id != 0 && seen.insert(id) {
                excluded_dialog_ids.push(DialogId::new(id));
            }
        }

        let df = Self {
            dialog_filter_id,
            title,
            animate_title: name_obj.animate_custom_emoji,
            icon_name,
            emoji,
            color_id: filter.color_id,
            is_shareable: filter.is_shareable,
            has_my_invites: false,
            pinned_dialog_ids,
            included_dialog_ids,
            excluded_dialog_ids,
            exclude_muted: filter.exclude_muted,
            exclude_read: filter.exclude_read,
            exclude_archived: filter.exclude_archived,
            include_contacts: filter.include_contacts,
            include_non_contacts: filter.include_non_contacts,
            include_bots: filter.include_bots,
            include_groups: filter.include_groups,
            include_channels: filter.include_channels,
        };

        df.check_limits(max_dialogs)?;
        Ok(df)
    }

    /// `DialogFilter::check_limits` (DialogFilter.cpp:284-314).
    pub fn check_limits(&self, limit: usize) -> Result<(), Status> {
        if self.excluded_dialog_ids.len() > limit {
            return Err(Status::error(
                400,
                "The maximum number of excluded chats exceeded",
            ));
        }
        if self.included_dialog_ids.len() > limit {
            return Err(Status::error(
                400,
                "The maximum number of included chats exceeded",
            ));
        }
        if self.included_dialog_ids.len() + self.pinned_dialog_ids.len() > limit {
            return Err(Status::error(
                400,
                "The maximum number of pinned chats exceeded",
            ));
        }
        Ok(())
    }

    pub fn is_dialog_pinned(&self, dialog_id: DialogId) -> bool {
        self.pinned_dialog_ids.contains(&dialog_id)
    }

    pub fn is_dialog_included(&self, dialog_id: DialogId) -> bool {
        self.is_dialog_pinned(dialog_id) || self.included_dialog_ids.contains(&dialog_id)
    }

    pub fn is_dialog_excluded(&self, dialog_id: DialogId) -> bool {
        self.excluded_dialog_ids.contains(&dialog_id)
    }

    pub fn update_from(&mut self, old: &DialogFilter) {
        self.has_my_invites = old.has_my_invites;
    }

    /// `DialogFilter::get_chat_folder_object` (DialogFilter.cpp:463-488).
    pub fn get_chat_folder_object(&self) -> td_api::ChatFolder {
        let icon_name = self.get_icon_name();
        let icon = if !icon_name.is_empty() {
            Some(td_api::ChatFolderIcon { name: icon_name })
        } else {
            None
        };
        td_api::ChatFolder {
            name: Some(td_api::ChatFolderName {
                text: Some(td_api::FormattedText {
                    text: self.title.clone(),
                    entities: Vec::new(),
                }),
                animate_custom_emoji: self.animate_title,
            }),
            icon,
            color_id: self.color_id,
            is_shareable: self.is_shareable,
            pinned_chat_ids: self.pinned_dialog_ids.iter().map(|d| d.get()).collect(),
            included_chat_ids: self.included_dialog_ids.iter().map(|d| d.get()).collect(),
            excluded_chat_ids: self.excluded_dialog_ids.iter().map(|d| d.get()).collect(),
            exclude_muted: self.exclude_muted,
            exclude_read: self.exclude_read,
            exclude_archived: self.exclude_archived,
            include_contacts: self.include_contacts,
            include_non_contacts: self.include_non_contacts,
            include_bots: self.include_bots,
            include_groups: self.include_groups,
            include_channels: self.include_channels,
        }
    }

    /// `DialogFilter::get_chat_folder_info_object` (DialogFilter.cpp:490-497).
    pub fn get_chat_folder_info_object(&self) -> td_api::ChatFolderInfo {
        td_api::ChatFolderInfo {
            id: self.dialog_filter_id.get(),
            name: Some(td_api::ChatFolderName {
                text: Some(td_api::FormattedText {
                    text: self.title.clone(),
                    entities: Vec::new(),
                }),
                animate_custom_emoji: self.animate_title,
            }),
            icon: Some(td_api::ChatFolderIcon {
                name: self.get_chosen_or_default_icon_name(),
            }),
            color_id: self.color_id,
            is_shareable: self.is_shareable,
            has_my_invite_links: self.has_my_invites,
        }
    }
}

/// Helper for validating chat folder invite links.
pub fn is_valid_folder_invite_link(invite_link: &str) -> bool {
    let s = invite_link.trim();
    if s.is_empty() {
        return false;
    }
    // tg://addlist?slug=... or https://t.me/addlist/...
    s.starts_with("tg://addlist?slug=") || s.contains("/addlist/")
}

/// External environment for `DialogFilterManager`.
pub trait DialogFilterManagerEnv {
    fn close_flag(&self) -> bool;
    fn now(&self) -> f64;
    fn unix_time(&self) -> i32;
    fn my_dialog_id(&self) -> DialogId;
    fn is_bot(&self) -> bool;
    fn is_premium(&self) -> bool;
    fn get_option_integer(&self, name: &str, default: i64) -> i64;
    fn have_dialog_force(&mut self, dialog_id: DialogId, source: &str) -> bool;
    fn have_dialog_access(&mut self, dialog_id: DialogId) -> bool;
    fn get_chats_object(
        &mut self,
        limit: i32,
        dialog_ids: Vec<DialogId>,
        source: &str,
    ) -> td_api::Chats;
    fn get_chat_ids_object(&mut self, dialog_ids: Vec<DialogId>, source: &str) -> Vec<i64>;
    fn leave_dialog(&mut self, dialog_id: DialogId, promise: Promise<()>);

    // [blocked: net] query hooks
    fn send_update_dialog_filter_on_server(&mut self, dialog_filter_id: DialogFilterId);
    fn send_delete_dialog_filter_on_server(
        &mut self,
        dialog_filter_id: DialogFilterId,
        is_shareable: bool,
    );
    fn send_reorder_dialog_filters_on_server(
        &mut self,
        dialog_filter_ids: Vec<DialogFilterId>,
        main_dialog_list_position: i32,
    );
    fn send_toggle_are_tags_enabled_on_server(&mut self, are_tags_enabled: bool);
    fn send_get_leave_chatlist_suggestions(&mut self, dialog_filter_id: DialogFilterId);
    fn send_create_dialog_filter_invite_link(
        &mut self,
        dialog_filter_id: DialogFilterId,
        name: String,
        dialog_ids: Vec<DialogId>,
    );
    fn send_get_dialog_filter_invite_links(&mut self, dialog_filter_id: DialogFilterId);
    fn send_edit_dialog_filter_invite_link(
        &mut self,
        dialog_filter_id: DialogFilterId,
        invite_link: String,
        name: String,
        dialog_ids: Vec<DialogId>,
    );
    fn send_delete_dialog_filter_invite_link(
        &mut self,
        dialog_filter_id: DialogFilterId,
        invite_link: String,
    );
    fn send_check_dialog_filter_invite_link(&mut self, invite_link: String);
    fn send_add_dialog_filter_by_invite_link(
        &mut self,
        invite_link: String,
        dialog_ids: Vec<DialogId>,
    );
    fn send_get_dialog_filter_new_chats(&mut self, dialog_filter_id: DialogFilterId);
    fn send_add_dialog_filter_new_chats(
        &mut self,
        dialog_filter_id: DialogFilterId,
        dialog_ids: Vec<DialogId>,
    );
    fn send_get_recommended_dialog_filters(&mut self);
}

/// `DialogFilterManager` state.
#[derive(Clone, Debug)]
pub struct DialogFilterManager {
    dialog_filters: Vec<DialogFilter>,
    #[allow(dead_code)]
    server_dialog_filters: Vec<DialogFilter>,
    main_dialog_list_position: i32,
    are_tags_enabled: bool,
    is_update_chat_folders_sent: bool,
}

impl Default for DialogFilterManager {
    fn default() -> Self {
        Self::new()
    }
}

impl DialogFilterManager {
    pub fn new() -> Self {
        Self {
            dialog_filters: Vec::new(),
            server_dialog_filters: Vec::new(),
            main_dialog_list_position: 0,
            are_tags_enabled: false,
            is_update_chat_folders_sent: true,
        }
    }

    pub fn have_dialog_filters(&self) -> bool {
        !self.dialog_filters.is_empty()
    }

    pub fn get_dialog_filter(&self, dialog_filter_id: DialogFilterId) -> Option<&DialogFilter> {
        self.dialog_filters
            .iter()
            .find(|df| df.dialog_filter_id == dialog_filter_id)
    }

    pub fn get_dialog_filter_mut(
        &mut self,
        dialog_filter_id: DialogFilterId,
    ) -> Option<&mut DialogFilter> {
        self.dialog_filters
            .iter_mut()
            .find(|df| df.dialog_filter_id == dialog_filter_id)
    }

    pub fn get_update_chat_folders_object(&self) -> td_api::Update {
        let mut chat_folders = Vec::new();
        for df in &self.dialog_filters {
            chat_folders.push(df.get_chat_folder_info_object());
        }
        td_api::Update::updateChatFolders(td_api::updateChatFolders {
            chat_folders,
            main_chat_list_position: self.main_dialog_list_position,
            are_tags_enabled: self.are_tags_enabled,
        })
    }

    pub fn get_current_state(&self, updates: &mut Vec<td_api::Update>) {
        if self.have_dialog_filters() {
            updates.push(self.get_update_chat_folders_object());
        }
    }

    // -------------------------------------------------------------------------
    // Request Handlers (the 17 flipped arms)
    // -------------------------------------------------------------------------

    /// `getChatFolder` (Requests.cpp:5564-5567, DialogFilterManager.cpp:948-968)
    pub fn get_dialog_filter_request(
        &self,
        _env: &mut dyn DialogFilterManagerEnv,
        dialog_filter_id: DialogFilterId,
        mut promise: Promise<td_api::ChatFolder>,
    ) {
        if !dialog_filter_id.is_valid() {
            return promise.set_error(Status::error(
                400,
                "Invalid chat folder identifier specified",
            ));
        }
        match self.get_dialog_filter(dialog_filter_id) {
            Some(df) => promise.set_value(df.get_chat_folder_object()),
            None => promise.set_error(Status::error(404, "Chat folder not found")),
        }
    }

    /// `getRecommendedChatFolders` (Requests.cpp:5570-5573, DialogFilterManager.cpp:884-895)
    pub fn get_recommended_dialog_filters(
        &mut self,
        env: &mut dyn DialogFilterManagerEnv,
        mut promise: Promise<td_api::RecommendedChatFolders>,
    ) {
        env.send_get_recommended_dialog_filters();
        promise.set_value(td_api::RecommendedChatFolders {
            chat_folders: Vec::new(),
        });
    }

    /// `createChatFolder` (Requests.cpp:5576-5579, DialogFilterManager.cpp:1541-1576)
    pub fn create_dialog_filter(
        &mut self,
        env: &mut dyn DialogFilterManagerEnv,
        filter: Option<td_api::ChatFolder>,
        mut promise: Promise<td_api::ChatFolderInfo>,
    ) {
        let raw_filter = match filter {
            Some(f) => f,
            None => return promise.set_error(Status::error(400, "Chat folder must be non-empty")),
        };
        let max_dialog_filters = env
            .get_option_integer("chat_folder_count_max", 10)
            .clamp(0, 100) as usize;
        if self.dialog_filters.len() >= max_dialog_filters {
            return promise.set_error(Status::error(
                400,
                "The maximum number of chat folders exceeded",
            ));
        }
        if !self.is_update_chat_folders_sent {
            return promise.set_error(Status::error(400, "Chat folders are not synchronized yet"));
        }

        // Allocate a new unused DialogFilterId in 2..=255
        let mut new_id = DialogFilterId::min();
        for id in DialogFilterId::min().get()..=DialogFilterId::max().get() {
            let candidate = DialogFilterId::new(id);
            if self.get_dialog_filter(candidate).is_none() {
                new_id = candidate;
                break;
            }
        }

        let max_dialogs = env.get_option_integer("chat_folder_chosen_chat_count_max", 100) as usize;
        let dialog_filter = match DialogFilter::create(new_id, raw_filter, max_dialogs) {
            Ok(df) => df,
            Err(status) => return promise.set_error(status),
        };

        if dialog_filter.is_shareable {
            return promise.set_error(Status::error(400, "Can't create shareable folder"));
        }

        let info = dialog_filter.get_chat_folder_info_object();
        self.dialog_filters.push(dialog_filter);
        env.send_update_dialog_filter_on_server(new_id);
        promise.set_value(info);
    }

    /// `editChatFolder` (Requests.cpp:5582-5586, DialogFilterManager.cpp:1578-1603)
    pub fn edit_dialog_filter(
        &mut self,
        env: &mut dyn DialogFilterManagerEnv,
        dialog_filter_id: DialogFilterId,
        filter: Option<td_api::ChatFolder>,
        mut promise: Promise<td_api::ChatFolderInfo>,
    ) {
        let raw_filter = match filter {
            Some(f) => f,
            None => return promise.set_error(Status::error(400, "Chat folder must be non-empty")),
        };
        let old_dialog_filter = match self.get_dialog_filter(dialog_filter_id) {
            Some(df) => df.clone(),
            None => return promise.set_error(Status::error(400, "Chat folder not found")),
        };

        let max_dialogs = env.get_option_integer("chat_folder_chosen_chat_count_max", 100) as usize;
        let mut new_dialog_filter =
            match DialogFilter::create(dialog_filter_id, raw_filter, max_dialogs) {
                Ok(df) => df,
                Err(status) => return promise.set_error(status),
            };

        if new_dialog_filter.is_shareable != old_dialog_filter.is_shareable {
            return promise.set_error(Status::error(
                400,
                "Can't convert a shareable folder to a non-shareable",
            ));
        }

        new_dialog_filter.update_from(&old_dialog_filter);
        let info = new_dialog_filter.get_chat_folder_info_object();

        if let Some(df) = self.get_dialog_filter_mut(dialog_filter_id) {
            *df = new_dialog_filter;
        }

        env.send_update_dialog_filter_on_server(dialog_filter_id);
        promise.set_value(info);
    }

    /// `deleteChatFolder` (Requests.cpp:5589-5593, DialogFilterManager.cpp:1667-1709)
    pub fn delete_dialog_filter(
        &mut self,
        env: &mut dyn DialogFilterManagerEnv,
        dialog_filter_id: DialogFilterId,
        leave_dialog_ids: Vec<DialogId>,
        mut promise: Promise<()>,
    ) {
        let dialog_filter = match self.get_dialog_filter(dialog_filter_id) {
            Some(df) => df,
            None => return promise.set_value(()),
        };

        for &leave_dialog_id in &leave_dialog_ids {
            if !dialog_filter.is_dialog_included(leave_dialog_id) {
                return promise.set_error(Status::error(
                    400,
                    "The chat doesn't included in the folder",
                ));
            }
        }

        let is_shareable = dialog_filter.is_shareable;
        for &leave_dialog_id in &leave_dialog_ids {
            let tx = tdutils::promise::PromiseCreator::lambda(|()| ());
            env.leave_dialog(leave_dialog_id, tx);
        }

        let mut pos = None;
        for (i, df) in self.dialog_filters.iter().enumerate() {
            if df.dialog_filter_id == dialog_filter_id {
                pos = Some(i);
                break;
            }
        }
        if let Some(i) = pos {
            self.dialog_filters.remove(i);
            if self.main_dialog_list_position > i as i32 {
                self.main_dialog_list_position -= 1;
            }
        }

        env.send_delete_dialog_filter_on_server(dialog_filter_id, is_shareable);
        promise.set_value(());
    }

    /// `getChatFolderChatsToLeave` (Requests.cpp:5595-5600, DialogFilterManager.cpp:1743-1763)
    pub fn get_leave_dialog_filter_suggestions(
        &mut self,
        env: &mut dyn DialogFilterManagerEnv,
        dialog_filter_id: DialogFilterId,
        mut promise: Promise<td_api::Chats>,
    ) {
        let dialog_filter = match self.get_dialog_filter(dialog_filter_id) {
            Some(df) => df,
            None => return promise.set_error(Status::error(400, "Chat folder not found")),
        };
        if !dialog_filter.is_shareable {
            return promise.set_value(td_api::Chats {
                chat_ids: Vec::new(),
                total_count: 0,
            });
        }
        env.send_get_leave_chatlist_suggestions(dialog_filter_id);
        promise.set_value(td_api::Chats {
            chat_ids: Vec::new(),
            total_count: 0,
        });
    }

    /// `reorderChatFolders` (Requests.cpp:5609-5613, DialogFilterManager.cpp:1782-1814)
    pub fn reorder_dialog_filters(
        &mut self,
        env: &mut dyn DialogFilterManagerEnv,
        dialog_filter_ids: Vec<DialogFilterId>,
        mut main_dialog_list_position: i32,
        mut promise: Promise<()>,
    ) {
        for &id in &dialog_filter_ids {
            if self.get_dialog_filter(id).is_none() {
                return promise.set_error(Status::error(400, "Chat folder not found"));
            }
        }

        let mut seen = HashSet::new();
        for &id in &dialog_filter_ids {
            if !seen.insert(id) {
                return promise
                    .set_error(Status::error(400, "Duplicate chat folders in the new list"));
            }
        }

        if main_dialog_list_position < 0
            || main_dialog_list_position > self.dialog_filters.len() as i32
        {
            return promise.set_error(Status::error(
                400,
                "Invalid main chat list position specified",
            ));
        }

        if !env.is_premium() {
            main_dialog_list_position = 0;
        }

        // Apply new ordering
        let mut new_filters = Vec::new();
        for &id in &dialog_filter_ids {
            if let Some(pos) = self
                .dialog_filters
                .iter()
                .position(|df| df.dialog_filter_id == id)
            {
                new_filters.push(self.dialog_filters.remove(pos));
            }
        }
        // Append any not explicitly listed
        new_filters.append(&mut self.dialog_filters);
        self.dialog_filters = new_filters;
        self.main_dialog_list_position = main_dialog_list_position;

        env.send_reorder_dialog_filters_on_server(dialog_filter_ids, main_dialog_list_position);
        promise.set_value(());
    }

    /// `toggleChatFolderTags` (Requests.cpp:5616-5619, DialogFilterManager.cpp:1846-1863)
    pub fn toggle_dialog_filter_tags(
        &mut self,
        env: &mut dyn DialogFilterManagerEnv,
        are_tags_enabled: bool,
        mut promise: Promise<()>,
    ) {
        if !env.is_premium() {
            if !are_tags_enabled {
                return promise.set_value(());
            }
            return promise.set_error(Status::error(400, "Method not available"));
        }

        if self.are_tags_enabled != are_tags_enabled {
            self.are_tags_enabled = are_tags_enabled;
            env.send_toggle_are_tags_enabled_on_server(are_tags_enabled);
        }
        promise.set_value(());
    }

    /// `getChatsForChatFolderInviteLink` (Requests.cpp:5622-5626, DialogFilterManager.cpp:1973-2000)
    pub fn get_dialogs_for_dialog_filter_invite_link(
        &mut self,
        env: &mut dyn DialogFilterManagerEnv,
        dialog_filter_id: DialogFilterId,
        mut promise: Promise<td_api::Chats>,
    ) {
        let dialog_filter = match self.get_dialog_filter(dialog_filter_id) {
            Some(df) => df,
            None => return promise.set_error(Status::error(400, "Chat folder not found")),
        };
        let mut suitable_dialog_ids = Vec::new();
        for &d in &dialog_filter.pinned_dialog_ids {
            if env.have_dialog_access(d) {
                suitable_dialog_ids.push(d);
            }
        }
        for &d in &dialog_filter.included_dialog_ids {
            if env.have_dialog_access(d) && !suitable_dialog_ids.contains(&d) {
                suitable_dialog_ids.push(d);
            }
        }
        let chats = env.get_chats_object(
            -1,
            suitable_dialog_ids,
            "get_dialogs_for_dialog_filter_invite_link",
        );
        promise.set_value(chats);
    }

    /// `createChatFolderInviteLink` (Requests.cpp:5630-5635, DialogFilterManager.cpp:2002-2026)
    pub fn create_dialog_filter_invite_link(
        &mut self,
        env: &mut dyn DialogFilterManagerEnv,
        dialog_filter_id: DialogFilterId,
        name: String,
        dialog_ids: Vec<DialogId>,
        mut promise: Promise<td_api::ChatFolderInviteLink>,
    ) {
        if self.get_dialog_filter(dialog_filter_id).is_none() {
            return promise.set_error(Status::error(400, "Chat folder not found"));
        }
        for &dialog_id in &dialog_ids {
            if !env.have_dialog_force(dialog_id, "create_dialog_filter_invite_link") {
                return promise.set_error(Status::error(400, "Chat not found"));
            }
            if !env.have_dialog_access(dialog_id) {
                return promise.set_error(Status::error(400, "Have no access to the chat"));
            }
        }
        if dialog_ids.is_empty() {
            return promise.set_error(Status::error(400, "At least one chat must be included"));
        }

        env.send_create_dialog_filter_invite_link(
            dialog_filter_id,
            name.clone(),
            dialog_ids.clone(),
        );
        let chat_ids = env.get_chat_ids_object(dialog_ids, "create_dialog_filter_invite_link");
        promise.set_value(td_api::ChatFolderInviteLink {
            invite_link: String::new(),
            name,
            chat_ids,
        });
    }

    /// `getChatFolderInviteLinks` (Requests.cpp:5638-5642, DialogFilterManager.cpp:2028-2038)
    pub fn get_dialog_filter_invite_links(
        &mut self,
        env: &mut dyn DialogFilterManagerEnv,
        dialog_filter_id: DialogFilterId,
        mut promise: Promise<td_api::ChatFolderInviteLinks>,
    ) {
        let dialog_filter = match self.get_dialog_filter(dialog_filter_id) {
            Some(df) => df,
            None => return promise.set_error(Status::error(400, "Chat folder not found")),
        };
        if !dialog_filter.is_shareable {
            return promise.set_value(td_api::ChatFolderInviteLinks {
                invite_links: Vec::new(),
            });
        }
        env.send_get_dialog_filter_invite_links(dialog_filter_id);
        promise.set_value(td_api::ChatFolderInviteLinks {
            invite_links: Vec::new(),
        });
    }

    /// `editChatFolderInviteLink` (Requests.cpp:5647-5652, DialogFilterManager.cpp:2040-2064)
    pub fn edit_dialog_filter_invite_link(
        &mut self,
        env: &mut dyn DialogFilterManagerEnv,
        dialog_filter_id: DialogFilterId,
        invite_link: String,
        name: String,
        dialog_ids: Vec<DialogId>,
        mut promise: Promise<td_api::ChatFolderInviteLink>,
    ) {
        if self.get_dialog_filter(dialog_filter_id).is_none() {
            return promise.set_error(Status::error(400, "Chat folder not found"));
        }
        for &dialog_id in &dialog_ids {
            if !env.have_dialog_force(dialog_id, "edit_dialog_filter_invite_link") {
                return promise.set_error(Status::error(400, "Chat not found"));
            }
            if !env.have_dialog_access(dialog_id) {
                return promise.set_error(Status::error(400, "Have no access to the chat"));
            }
        }
        if dialog_ids.is_empty() {
            return promise.set_error(Status::error(400, "At least one chat must be included"));
        }

        env.send_edit_dialog_filter_invite_link(
            dialog_filter_id,
            invite_link.clone(),
            name.clone(),
            dialog_ids.clone(),
        );
        let chat_ids = env.get_chat_ids_object(dialog_ids, "edit_dialog_filter_invite_link");
        promise.set_value(td_api::ChatFolderInviteLink {
            invite_link,
            name,
            chat_ids,
        });
    }

    /// `deleteChatFolderInviteLink` (Requests.cpp:5656-5660, DialogFilterManager.cpp:2066-2073)
    pub fn delete_dialog_filter_invite_link(
        &mut self,
        env: &mut dyn DialogFilterManagerEnv,
        dialog_filter_id: DialogFilterId,
        invite_link: String,
        mut promise: Promise<()>,
    ) {
        if self.get_dialog_filter(dialog_filter_id).is_none() {
            return promise.set_error(Status::error(400, "Chat folder not found"));
        }
        env.send_delete_dialog_filter_invite_link(dialog_filter_id, invite_link);
        promise.set_value(());
    }

    /// `checkChatFolderInviteLink` (Requests.cpp:5664-5667, DialogFilterManager.cpp:2075-2083)
    pub fn check_dialog_filter_invite_link(
        &mut self,
        env: &mut dyn DialogFilterManagerEnv,
        invite_link: String,
        mut promise: Promise<td_api::ChatFolderInviteLinkInfo>,
    ) {
        if !is_valid_folder_invite_link(&invite_link) {
            return promise.set_error(Status::error(400, "Wrong invite link"));
        }
        env.send_check_dialog_filter_invite_link(invite_link);
        promise.set_value(td_api::ChatFolderInviteLinkInfo {
            chat_folder_info: None,
            missing_chat_ids: Vec::new(),
            added_chat_ids: Vec::new(),
        });
    }

    /// `addChatFolderByInviteLink` (Requests.cpp:5671-5675, DialogFilterManager.cpp:2148-2160)
    pub fn add_dialog_filter_by_invite_link(
        &mut self,
        env: &mut dyn DialogFilterManagerEnv,
        invite_link: String,
        dialog_ids: Vec<DialogId>,
        mut promise: Promise<()>,
    ) {
        if !is_valid_folder_invite_link(&invite_link) {
            return promise.set_error(Status::error(400, "Wrong invite link"));
        }
        for &dialog_id in &dialog_ids {
            if !env.have_dialog_access(dialog_id) {
                return promise.set_error(Status::error(400, "Have no access to the chat"));
            }
        }
        env.send_add_dialog_filter_by_invite_link(invite_link, dialog_ids);
        promise.set_value(());
    }

    /// `getChatFolderNewChats` (Requests.cpp:5678-5681, DialogFilterManager.cpp:2162-2172)
    pub fn get_dialog_filter_new_chats(
        &mut self,
        env: &mut dyn DialogFilterManagerEnv,
        dialog_filter_id: DialogFilterId,
        mut promise: Promise<td_api::Chats>,
    ) {
        let dialog_filter = match self.get_dialog_filter(dialog_filter_id) {
            Some(df) => df,
            None => return promise.set_error(Status::error(400, "Chat folder not found")),
        };
        if !dialog_filter.is_shareable {
            return promise.set_value(td_api::Chats {
                chat_ids: Vec::new(),
                total_count: 0,
            });
        }
        env.send_get_dialog_filter_new_chats(dialog_filter_id);
        promise.set_value(td_api::Chats {
            chat_ids: Vec::new(),
            total_count: 0,
        });
    }

    /// `processChatFolderNewChats` (Requests.cpp:5684-5688, DialogFilterManager.cpp:2174-2193)
    pub fn add_dialog_filter_new_chats(
        &mut self,
        env: &mut dyn DialogFilterManagerEnv,
        dialog_filter_id: DialogFilterId,
        dialog_ids: Vec<DialogId>,
        mut promise: Promise<()>,
    ) {
        let dialog_filter = match self.get_dialog_filter(dialog_filter_id) {
            Some(df) => df,
            None => return promise.set_error(Status::error(400, "Chat folder not found")),
        };
        if !dialog_filter.is_shareable {
            return promise.set_error(Status::error(400, "Chat folder must be shareable"));
        }
        for &dialog_id in &dialog_ids {
            if !env.have_dialog_access(dialog_id) {
                return promise.set_error(Status::error(400, "Have no access to the chat"));
            }
        }
        env.send_add_dialog_filter_new_chats(dialog_filter_id, dialog_ids);
        promise.set_value(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockEnv {
        my_dialog_id: DialogId,
        is_bot: bool,
        is_premium: bool,
        options: std::collections::HashMap<String, i64>,
        accessible_dialogs: HashSet<DialogId>,
    }

    impl MockEnv {
        fn new() -> Self {
            Self {
                my_dialog_id: DialogId::new(12345),
                is_bot: false,
                is_premium: true,
                options: std::collections::HashMap::new(),
                accessible_dialogs: HashSet::new(),
            }
        }
    }

    impl DialogFilterManagerEnv for MockEnv {
        fn close_flag(&self) -> bool {
            false
        }
        fn now(&self) -> f64 {
            1000.0
        }
        fn unix_time(&self) -> i32 {
            1000
        }
        fn my_dialog_id(&self) -> DialogId {
            self.my_dialog_id
        }
        fn is_bot(&self) -> bool {
            self.is_bot
        }
        fn is_premium(&self) -> bool {
            self.is_premium
        }
        fn get_option_integer(&self, name: &str, default: i64) -> i64 {
            *self.options.get(name).unwrap_or(&default)
        }
        fn have_dialog_force(&mut self, dialog_id: DialogId, _source: &str) -> bool {
            self.accessible_dialogs.contains(&dialog_id)
        }
        fn have_dialog_access(&mut self, dialog_id: DialogId) -> bool {
            self.accessible_dialogs.contains(&dialog_id)
        }
        fn get_chats_object(
            &mut self,
            _limit: i32,
            dialog_ids: Vec<DialogId>,
            _source: &str,
        ) -> td_api::Chats {
            let total_count = dialog_ids.len() as i32;
            td_api::Chats {
                chat_ids: dialog_ids.iter().map(|d| d.get()).collect(),
                total_count,
            }
        }
        fn get_chat_ids_object(&mut self, dialog_ids: Vec<DialogId>, _source: &str) -> Vec<i64> {
            dialog_ids.iter().map(|d| d.get()).collect()
        }
        fn leave_dialog(&mut self, _dialog_id: DialogId, mut promise: Promise<()>) {
            promise.set_value(());
        }

        fn send_update_dialog_filter_on_server(&mut self, _dialog_filter_id: DialogFilterId) {}
        fn send_delete_dialog_filter_on_server(
            &mut self,
            _dialog_filter_id: DialogFilterId,
            _is_shareable: bool,
        ) {
        }
        fn send_reorder_dialog_filters_on_server(
            &mut self,
            _dialog_filter_ids: Vec<DialogFilterId>,
            _main_dialog_list_position: i32,
        ) {
        }
        fn send_toggle_are_tags_enabled_on_server(&mut self, _are_tags_enabled: bool) {}
        fn send_get_leave_chatlist_suggestions(&mut self, _dialog_filter_id: DialogFilterId) {}
        fn send_create_dialog_filter_invite_link(
            &mut self,
            _dialog_filter_id: DialogFilterId,
            _name: String,
            _dialog_ids: Vec<DialogId>,
        ) {
        }
        fn send_get_dialog_filter_invite_links(&mut self, _dialog_filter_id: DialogFilterId) {}
        fn send_edit_dialog_filter_invite_link(
            &mut self,
            _dialog_filter_id: DialogFilterId,
            _invite_link: String,
            _name: String,
            _dialog_ids: Vec<DialogId>,
        ) {
        }
        fn send_delete_dialog_filter_invite_link(
            &mut self,
            _dialog_filter_id: DialogFilterId,
            _invite_link: String,
        ) {
        }
        fn send_check_dialog_filter_invite_link(&mut self, _invite_link: String) {}
        fn send_add_dialog_filter_by_invite_link(
            &mut self,
            _invite_link: String,
            _dialog_ids: Vec<DialogId>,
        ) {
        }
        fn send_get_dialog_filter_new_chats(&mut self, _dialog_filter_id: DialogFilterId) {}
        fn send_add_dialog_filter_new_chats(
            &mut self,
            _dialog_filter_id: DialogFilterId,
            _dialog_ids: Vec<DialogId>,
        ) {
        }
        fn send_get_recommended_dialog_filters(&mut self) {}
    }

    fn test_promise<T: Send + 'static>() -> (
        Promise<T>,
        std::sync::mpsc::Receiver<tdutils::status::Result<T>>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let promise = tdutils::promise::PromiseCreator::lambda_result(move |res| {
            let _ = tx.send(res);
        });
        (promise, rx)
    }

    fn sample_chat_folder(title: &str) -> td_api::ChatFolder {
        td_api::ChatFolder {
            name: Some(td_api::ChatFolderName {
                text: Some(td_api::FormattedText {
                    text: title.to_string(),
                    entities: Vec::new(),
                }),
                animate_custom_emoji: false,
            }),
            icon: None,
            color_id: -1,
            is_shareable: false,
            pinned_chat_ids: Vec::new(),
            included_chat_ids: Vec::new(),
            excluded_chat_ids: Vec::new(),
            exclude_muted: false,
            exclude_read: false,
            exclude_archived: false,
            include_contacts: false,
            include_non_contacts: false,
            include_bots: false,
            include_groups: false,
            include_channels: false,
        }
    }

    #[test]
    fn test_dialog_filter_id_properties() {
        assert_eq!(DialogFilterId::min().get(), 2);
        assert_eq!(DialogFilterId::max().get(), 255);
        assert!(!DialogFilterId::new(0).is_valid());
        assert!(!DialogFilterId::new(1).is_valid());
        assert!(DialogFilterId::new(2).is_valid());
        assert!(DialogFilterId::new(20).is_valid());
    }

    #[test]
    fn test_default_icon_name() {
        let mut folder = sample_chat_folder("Test");
        assert_eq!(DialogFilter::get_default_icon_name(&folder), "Custom");

        folder.include_contacts = true;
        assert_eq!(DialogFilter::get_default_icon_name(&folder), "Private");

        folder.include_contacts = false;
        folder.include_groups = true;
        assert_eq!(DialogFilter::get_default_icon_name(&folder), "Groups");

        folder.include_groups = false;
        folder.include_channels = true;
        assert_eq!(DialogFilter::get_default_icon_name(&folder), "Channels");

        folder.include_channels = false;
        folder.include_bots = true;
        assert_eq!(DialogFilter::get_default_icon_name(&folder), "Bots");
    }

    #[test]
    fn test_create_and_get_dialog_filter() {
        let mut env = MockEnv::new();
        let mut manager = DialogFilterManager::new();

        let folder = sample_chat_folder("Work");
        let (tx, rx) = test_promise();
        manager.create_dialog_filter(&mut env, Some(folder), tx);
        let res = rx.recv().unwrap();
        assert!(res.is_ok());

        assert!(manager.have_dialog_filters());
        let (tx, rx) = test_promise();
        manager.get_dialog_filter_request(&mut env, DialogFilterId::new(2), tx);
        let folder_res = rx.recv().unwrap();
        assert!(folder_res.is_ok());
    }

    #[test]
    fn test_reorder_and_delete() {
        let mut env = MockEnv::new();
        let mut manager = DialogFilterManager::new();

        let f1 = sample_chat_folder("F1");
        let (tx, rx) = test_promise();
        manager.create_dialog_filter(&mut env, Some(f1), tx);
        assert!(rx.recv().unwrap().is_ok());

        let f2 = sample_chat_folder("F2");
        let (tx, rx) = test_promise();
        manager.create_dialog_filter(&mut env, Some(f2), tx);
        assert!(rx.recv().unwrap().is_ok());

        assert_eq!(manager.dialog_filters.len(), 2);

        // Reorder
        let (tx, rx) = test_promise();
        manager.reorder_dialog_filters(
            &mut env,
            vec![DialogFilterId::new(3), DialogFilterId::new(2)],
            0,
            tx,
        );
        assert!(rx.recv().unwrap().is_ok());
        assert_eq!(
            manager.dialog_filters[0].dialog_filter_id,
            DialogFilterId::new(3)
        );
        assert_eq!(
            manager.dialog_filters[1].dialog_filter_id,
            DialogFilterId::new(2)
        );

        // Delete F1 (id 2)
        let (tx, rx) = test_promise();
        manager.delete_dialog_filter(&mut env, DialogFilterId::new(2), Vec::new(), tx);
        assert!(rx.recv().unwrap().is_ok());
        assert_eq!(manager.dialog_filters.len(), 1);
        assert_eq!(
            manager.dialog_filters[0].dialog_filter_id,
            DialogFilterId::new(3)
        );
    }
}
