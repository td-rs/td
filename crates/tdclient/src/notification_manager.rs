//! Port of `td/telegram/NotificationManager.{h,cpp}` (M.13): the active
//! notification groups, the update diff/flush machinery, the id spaces and
//! the pmc persistence. Anchored on the local C++ checkout, which is newer
//! than 1.8.67 (see [`crate::notification`] for the drift notes).
//!
//! Engine seams (each flips to a real manager intake when that slice
//! lands; the C++ line references name the flip target):
//! - [`NotificationManagerEnv`] — the `G()->`/`td_->` environment surface
//!   (clock, options, pmc, authorization) plus the cross-manager intakes:
//!   `get_message_object`/`get_chat_id_object`/`get_message_notification_group_force`
//!   (MessagesManager, M.10/M.13.2) and the push media objects (M.12 and
//!   the media managers). The default answers mirror the C++ "not loaded"
//!   results.
//! - The contact-registered sync (`run_contact_registered_notifications_sync`,
//!   cpp:2693) rides the `NotificationNetKind` plane (net-drain 61): the
//!   `account.{get,set}ContactSignUpNotification` queries dispatch over
//!   the manager-held handle and the answers drain through
//!   `Td::on_notification_net_answer` into the landed sync state machine.
//! - The push decrypt/run plane (`process_push_notification`, cpp:2146-2166)
//!   stays a parked seam — the C++ tail is the local decrypt +
//!   `process_push_notification_payload` value plane, not a wire family.
//! - The StateManager online callback (cpp:301-314) is dropped until the
//!   StateManager lands; `flush_all_pending_notifications` is the flip
//!   target.
//! - The database loaders (`load_message_notification_groups_from_database`,
//!   `load_notifications_from_database`) are ported, but their database
//!   intakes are env seams: the group-key paging SQL lives in
//!   [`crate::notification_db`] and the notification/message loads wait for
//!   the M.6 dialog-db plumbing and the M.10 MessagesManager.
//! - `tick_timeouts` lands with M.13.6/M.13.7, when its dispatch targets
//!   (`after_get_difference_impl`/`after_get_chat_difference_impl`) exist;
//!   the timeout maps and `next_deadline` are already real.
//!
//! Documented divergences (all synchronous-collapse, the M.3/M.4 pmc
//! convention):
//! - C++ completes push-notification promises asynchronously through
//!   `send_closure`; ours complete synchronously at the same decision
//!   points.
//! - The pmc state reads use a strict canonical-form int32 parse; the C++
//!   `to_integer<int32>` is a lenient prefix parse. Values are self-written
//!   (`to_string(int32)`), so intact databases parse identically; a corrupt
//!   value logs an error and reads as 0 instead of C++'s prefix value.

use std::collections::{BTreeMap, HashMap, HashSet};

use tdutils::promise::Promise;
use tdutils::status::Status;

/// The in-flight query kinds of the net-drain 61 contact-sign-up
/// families — the C++ `SetContactSignUpNotificationQuery`/
/// `GetContactSignUpNotificationQuery` (NotificationManager.cpp:73-127)
/// collapsed onto dispatcher tags; the answers drain through
/// `Td::on_notification_net_answer`. The set kind carries the promise
/// lambda's send-time `is_disabled` capture (cpp:2708-2710) for the
/// `on_contact_registered_notifications_sync` routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotificationNetKind {
    /// `SetContactSignUpNotificationQuery` (cpp:73-99) — any ok answer
    /// (Bool true or false) completes the sync; the capture routes the
    /// answer walk.
    SetContactSignUpNotification { is_disabled: bool },
    /// `GetContactSignUpNotificationQuery` (cpp:101-127) — the Bool
    /// answer feeds `on_get_disable_contact_registered_notifications`.
    GetContactSignUpNotification,
}

use crate::message_id::{DialogId, DialogType, MessageId};
use crate::notification::{
    CallId, Notification, NotificationGroupFromDatabase, NotificationGroupId, NotificationGroupKey,
    NotificationGroupType, NotificationId, NotificationObjectFullId, NotificationObjectId,
    NotificationType, NotificationTypeEnv, get_notification_object,
};
use crate::td_api;
use crate::user_manager::{FileId, UserId};

pub const MIN_NOTIFICATION_GROUP_COUNT_MAX: i32 = 0;
pub const MAX_NOTIFICATION_GROUP_COUNT_MAX: i32 = 25;
pub const MIN_NOTIFICATION_GROUP_SIZE_MAX: i32 = 1;
pub const MAX_NOTIFICATION_GROUP_SIZE_MAX: i32 = 25;

const DEFAULT_GROUP_COUNT_MAX: i32 = 0;
const DEFAULT_GROUP_SIZE_MAX: i32 = 10;
const EXTRA_GROUP_SIZE: usize = 10;

const DEFAULT_ONLINE_CLOUD_TIMEOUT_MS: i32 = 300_000;
const DEFAULT_ONLINE_CLOUD_DELAY_MS: i32 = 30_000;
const DEFAULT_DEFAULT_DELAY_MS: i32 = 1_500;

const MIN_NOTIFICATION_DELAY_MS: i32 = 1;

const MIN_UPDATE_DELAY_MS: i32 = 50;
const MAX_UPDATE_DELAY_MS: i32 = 60_000;

/// `NotificationManager::MAX_CALL_NOTIFICATION_GROUPS` (NotificationManager.h:143).
const MAX_CALL_NOTIFICATION_GROUPS: usize = 10;
/// `NotificationManager::MAX_CALL_NOTIFICATIONS` (NotificationManager.h:144).
const MAX_CALL_NOTIFICATIONS: usize = 10;

const ANNOUNCEMENT_ID_CACHE_TIME: i32 = 7 * 86_400;

/// `get_is_contact_registered_notifications_synchronized_key` (cpp:197).
pub const CONTACT_REGISTERED_SYNC_KEY: &str = "notifications_contact_registered_sync_state";
/// The pmc key holding `current_notification_id_` (cpp:238).
pub const NOTIFICATION_ID_CURRENT_KEY: &str = "notification_id_current";
/// The pmc key holding `current_notification_group_id_` (cpp:240).
pub const NOTIFICATION_GROUP_ID_CURRENT_KEY: &str = "notification_group_id_current";
/// The pmc key holding the call notification group ids (cpp:255).
pub const NOTIFICATION_CALL_GROUP_IDS_KEY: &str = "notification_call_group_ids";
/// The pmc key holding the cached announcement ids (cpp:285).
pub const NOTIFICATION_ANNOUNCEMENT_IDS_KEY: &str = "notification_announcement_ids";

/// `td::MyOnlineStatus` (OnlineManager.h) — the online-state snapshot
/// `get_notification_delay_ms` reads.
#[derive(Clone, Copy, Debug, Default)]
pub struct MyOnlineStatus {
    pub is_online_local: bool,
    pub is_online_remote: bool,
    pub was_online_local: f64,
    pub was_online_remote: f64,
}

/// The `G()`/`td_` surface of the manager. The cross-manager arms are seams
/// answering the C++ "not loaded" results until their managers land.
pub trait NotificationManagerEnv: NotificationTypeEnv {
    /// `G()->close_flag() || !td_->auth_manager_->is_authorized() ||
    /// td_->auth_manager_->is_bot()` (cpp:170-172).
    fn is_disabled(&self) -> bool;

    /// `DeviceTokenManager::get_actor_unsafe()->get_encryption_keys()` +
    /// the push decrypt/run plane (cpp:2146-2166) — `[blocked:
    /// push-plane]`: the C++ tail is the local decrypt +
    /// `process_push_notification_payload` value plane (no wire query);
    /// the seam parks the promise until that plane lands.
    fn send_process_push_notification_query(&mut self, _receiver_id: i64) {
        tdutils::log_info!("Process the push decrypt/run plane — [blocked: push-plane]");
    }

    /// `td_->option_manager_->set_option_boolean` — the
    /// `disable_contact_registered_notifications` write
    /// (NotificationManager.cpp:2673).
    fn set_contact_option_boolean(&mut self, _name: &str, _value: bool) {}

    /// `td_->option_manager_->set_option_empty` — the
    /// `disable_contact_registered_notifications` clear (cpp:2675).
    fn set_contact_option_empty(&mut self, _name: &str) {}

    /// `G()->close_flag()` — used separately from [`Self::is_disabled`].
    fn close_flag(&self) -> bool;

    /// `td_->option_manager_->get_option_integer`.
    fn get_option_integer(&self, name: &str, default_value: i64) -> i64;

    /// `td_->option_manager_->get_option_boolean`.
    fn get_option_boolean(&self, name: &str, default_value: bool) -> bool;

    /// `G()->unix_time()`.
    fn unix_time(&self) -> i32;

    /// `Time::now()` — the monotonic clock for the two timeout families.
    fn now(&self) -> f64;

    /// `G()->server_time()` — defaults to the unix time, its base.
    fn server_time(&self) -> f64 {
        self.unix_time() as f64
    }

    /// `td_->user_manager_->get_my_online_status` — seam until the
    /// UserManager online state lands; the default is offline everywhere.
    fn get_my_online_status(&self) -> MyOnlineStatus {
        MyOnlineStatus::default()
    }

    /// `G()->use_message_database()`.
    fn use_message_database(&self) -> bool {
        true
    }

    /// `send_closure(G()->td(), &Td::send_update, ...)`.
    fn send_update(&mut self, update: td_api::Update);

    /// `G()->td_db()->get_binlog_pmc()->get` — takes `&mut self` because
    /// the collapsed Td reaches the binlog pmc through `&mut Global`.
    fn pmc_get(&mut self, key: &str) -> String;

    /// `G()->td_db()->get_binlog_pmc()->set`.
    fn pmc_set(&mut self, key: &str, value: &str);

    /// `G()->td_db()->get_binlog_pmc()->erase`.
    fn pmc_erase(&mut self, key: &str);

    /// `G()->td_db()->get_binlog()->erase(event_id)` — the binlog events
    /// are the push-notification log events, which cannot exist until the
    /// push pipeline lands; the seam is a no-op.
    fn binlog_erase(&mut self, _event_id: u64) {}

    /// `td_->messages_manager_->get_chat_id_object` — seam until M.10; the
    /// bare dialog id is the thread-free answer.
    fn get_chat_id_object(&self, dialog_id: DialogId) -> i64 {
        dialog_id.get()
    }

    /// `td_->messages_manager_->get_message_notification_group_force`
    /// (cpp:421) — seam until M.13.2; `None` = the invalid-dialog answer.
    fn get_message_notification_group_force(
        &self,
        _group_id: NotificationGroupId,
    ) -> Option<NotificationGroupFromDatabase> {
        None
    }

    /// `td_->messages_manager_->get_message_notification_group_keys_from_database`
    /// (cpp:478) — the dialog-db paging query; the seam returns nothing
    /// until the M.6 dialog-db plumbing lands (the SQL is ported in
    /// [`crate::notification_db`]).
    fn get_message_notification_group_keys_from_database(
        &self,
        _from_group_key: NotificationGroupKey,
        _limit: i32,
    ) -> Vec<NotificationGroupKey> {
        Vec::new()
    }

    /// `td_->messages_manager_->get_message_notifications_from_database`
    /// (cpp:591) — the per-notification message-db load; the collapsed form
    /// answers synchronously, the seam returns an empty result.
    fn get_message_notifications_from_database(
        &self,
        _dialog_id: DialogId,
        _group_id: NotificationGroupId,
        _from_notification_id: NotificationId,
        _from_message_id: MessageId,
        _limit: i32,
    ) -> Result<Vec<Notification>, Status> {
        Ok(Vec::new())
    }

    /// `td_->dialog_manager_->force_create_dialog` (cpp:923) — seam until
    /// the DialogManager lands.
    fn force_create_dialog(&mut self, _dialog_id: DialogId, _source: &str, _force: bool) {}

    /// `td_->messages_manager_->remove_message_notification` — seam until
    /// M.10; the fan-out is a no-op until MessagesManager lands.
    fn remove_message_notification(
        &mut self,
        _dialog_id: DialogId,
        _group_id: NotificationGroupId,
        _notification_id: NotificationId,
    ) {
    }

    /// `td_->messages_manager_->remove_message_notifications` — seam until M.10.
    fn remove_message_notifications(
        &mut self,
        _dialog_id: DialogId,
        _group_id: NotificationGroupId,
        _max_notification_id: NotificationId,
        _max_message_id: MessageId,
    ) {
    }

    /// `td_->file_manager_->delete_file` — seam until the Files step.
    fn delete_file(&mut self, _file_id: FileId) {}
}

/// `NotificationManager::PendingNotification` (NotificationManager.h:160-174).
#[derive(Clone, Debug)]
pub struct PendingNotification {
    pub date: i32,
    pub settings_dialog_id: DialogId,
    pub disable_notification: bool,
    pub ringtone_id: i64,
    pub notification_id: NotificationId,
    pub r#type: NotificationType,
}

/// `NotificationManager::NotificationGroup` (NotificationManager.h:176-196);
/// the C++ default group type is `Calls` (h:178).
#[derive(Clone, Debug)]
pub struct NotificationGroup {
    pub total_count: i32,
    pub r#type: NotificationGroupType,
    pub is_loaded_from_database: bool,
    pub is_being_loaded_from_database: bool,
    pub notifications: Vec<Notification>,
    pub pending_notifications_flush_time: f64,
    pub pending_notifications: Vec<PendingNotification>,
}

impl Default for NotificationGroup {
    fn default() -> NotificationGroup {
        NotificationGroup {
            total_count: 0,
            r#type: NotificationGroupType::Calls,
            is_loaded_from_database: false,
            is_being_loaded_from_database: false,
            notifications: Vec::new(),
            pending_notifications_flush_time: 0.0,
            pending_notifications: Vec::new(),
        }
    }
}

/// `NotificationManager::SyncState` (NotificationManager.h:206).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SyncState {
    #[default]
    NotSynced,
    Pending,
    Completed,
}

/// `NotificationManager::TemporaryNotification` (NotificationManager.h:399-406).
#[derive(Clone, Debug)]
pub struct TemporaryNotification {
    pub group_id: NotificationGroupId,
    pub notification_id: NotificationId,
    pub sender_user_id: UserId,
    pub sender_dialog_id: DialogId,
    pub sender_name: String,
    pub is_outgoing: bool,
}

/// `NotificationManager::ActiveCallNotification` (NotificationManager.h:411-414).
#[derive(Clone, Copy, Debug)]
pub struct ActiveCallNotification {
    pub call_id: crate::notification::CallId,
    pub notification_id: NotificationId,
}

/// `td::NotificationManager` — the collapsed manager. The two C++
/// `MultiTimeout` members are deadline maps (key → absolute time);
/// `next_deadline` feeds the Td timeout slot and `tick_timeouts` fires
/// the due entries (see the module doc for the staging note).
pub struct NotificationManager {
    pub current_notification_id: NotificationId,
    pub current_notification_group_id: NotificationGroupId,

    pub max_notification_group_count: usize,
    pub max_notification_group_size: usize,
    pub keep_notification_group_size: usize,

    pub online_cloud_timeout_ms: i32,
    pub notification_cloud_delay_ms: i32,
    pub notification_default_delay_ms: i32,

    pub delayed_notification_update_count: i32,
    pub unreceived_notification_update_count: i32,

    pub last_loaded_notification_group_key: NotificationGroupKey,

    pub contact_registered_notifications_sync_state: SyncState,
    pub disable_contact_registered_notifications: bool,

    pub is_being_destroyed: bool,
    pub is_destroyed: bool,

    pub is_inited: bool,
    pub is_binlog_processed: bool,

    pub running_get_difference: bool,
    pub running_get_chat_difference: HashSet<i32>,

    pub groups: BTreeMap<NotificationGroupKey, NotificationGroup>,
    pub group_keys: HashMap<NotificationGroupId, NotificationGroupKey>,

    pub pending_updates: HashMap<i32, Vec<Option<td_api::Update>>>,

    pub flush_pending_notifications_timeout: BTreeMap<i64, f64>,
    pub flush_pending_updates_timeout: BTreeMap<i32, f64>,

    pub call_notification_group_ids: Vec<NotificationGroupId>,
    pub available_call_notification_group_ids: HashSet<NotificationGroupId>,
    pub dialog_id_to_call_notification_group_id: HashMap<DialogId, NotificationGroupId>,

    pub temporary_notification_log_event_ids: HashMap<NotificationId, u64>,
    pub temporary_edit_notification_log_event_ids: HashMap<NotificationId, u64>,
    pub temporary_notifications: HashMap<NotificationObjectFullId, TemporaryNotification>,
    pub temporary_notification_object_ids: HashMap<NotificationId, NotificationObjectFullId>,
    pub push_notification_promises: HashMap<NotificationId, Vec<Promise<()>>>,

    /// The `()`-typed promises parked at the process-push decrypt/run
    /// seam (the net_blocked_promises convention).
    pub parked_process_push_promises: Vec<Promise<()>>,

    pub active_call_notifications: HashMap<DialogId, Vec<ActiveCallNotification>>,

    pub announcement_id_date: HashMap<i32, i32>,

    /// The manager-held dispatch surface (the net-drain 61
    /// contact-sign-up families; `UserManager::net` shape) — `None`
    /// keeps the net-inert posture (the pre-init windows and the
    /// net-inert tests).
    pub(crate) net: Option<crate::td::NotificationNetHandle>,
    /// The parked `getOption(disable_contact_registered_notifications)`
    /// requests (the C++ `get_disable_contact_registered_notifications`
    /// parks the wrap_promise'd td_api promise, cpp:2729-2744; the port's
    /// request plumbing is the id) — FIFO-paired with the
    /// `GetContactSignUpNotification` answers.
    pub(crate) pending_contact_sign_up_get_options: Vec<u64>,
}

impl Default for NotificationManager {
    fn default() -> NotificationManager {
        NotificationManager {
            current_notification_id: NotificationId::default(),
            current_notification_group_id: NotificationGroupId::default(),
            max_notification_group_count: 0,
            max_notification_group_size: 0,
            keep_notification_group_size: 0,
            online_cloud_timeout_ms: DEFAULT_ONLINE_CLOUD_TIMEOUT_MS,
            notification_cloud_delay_ms: DEFAULT_ONLINE_CLOUD_DELAY_MS,
            notification_default_delay_ms: DEFAULT_DEFAULT_DELAY_MS,
            delayed_notification_update_count: 0,
            unreceived_notification_update_count: 0,
            last_loaded_notification_group_key: NotificationGroupKey::default(),
            contact_registered_notifications_sync_state: SyncState::NotSynced,
            disable_contact_registered_notifications: false,
            is_being_destroyed: false,
            is_destroyed: false,
            is_inited: false,
            is_binlog_processed: false,
            running_get_difference: false,
            running_get_chat_difference: HashSet::new(),
            groups: BTreeMap::new(),
            group_keys: HashMap::new(),
            pending_updates: HashMap::new(),
            flush_pending_notifications_timeout: BTreeMap::new(),
            flush_pending_updates_timeout: BTreeMap::new(),
            call_notification_group_ids: Vec::new(),
            available_call_notification_group_ids: HashSet::new(),
            dialog_id_to_call_notification_group_id: HashMap::new(),
            temporary_notification_log_event_ids: HashMap::new(),
            temporary_edit_notification_log_event_ids: HashMap::new(),
            temporary_notifications: HashMap::new(),
            temporary_notification_object_ids: HashMap::new(),
            push_notification_promises: HashMap::new(),
            parked_process_push_promises: Vec::new(),
            active_call_notifications: HashMap::new(),
            announcement_id_date: HashMap::new(),
            net: None,
            pending_contact_sign_up_get_options: Vec::new(),
        }
    }
}

impl NotificationManager {
    /// `NotificationManager::start_up` (cpp:201-203).
    pub fn start_up(&mut self, env: &mut impl NotificationManagerEnv) {
        self.init(env);
    }

    /// `NotificationManager::process_push_notification`
    /// (NotificationManager.cpp:2124-2160) — the disabled/"{}" band and
    /// the push-receiver-id parse are real; the encryption-key
    /// decrypt/run plane is the `[blocked: push-plane]` seam (the local
    /// decrypt + payload value plane, no wire query) and the promise
    /// parks (the wrapping lambda's error-200 mapping inlines to the OK
    /// answer here).
    pub fn process_push_notification(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        payload: String,
        mut promise: Promise<()>,
    ) {
        if env.is_disabled() || payload == "{}" {
            // the C++ `promise.set_error(200, "Immediate success")` maps
            // to the user-promise OK over its wrapping lambda
            promise.set_value(());
            return;
        }
        let receiver_id = match get_push_receiver_id(&payload) {
            Ok(receiver_id) => receiver_id,
            Err((code, message)) => return promise.set_error(Status::error(code, &message)),
        };
        env.send_process_push_notification_query(receiver_id);
        self.parked_process_push_promises.push(promise);
    }

    /// `NotificationManager::init` (cpp:205-315).
    pub fn init(&mut self, env: &mut impl NotificationManagerEnv) {
        if env.is_disabled() || self.is_inited {
            return;
        }

        self.disable_contact_registered_notifications =
            env.get_option_boolean("disable_contact_registered_notifications", false);
        let mut sync_state = env.pmc_get(CONTACT_REGISTERED_SYNC_KEY);
        if sync_state.is_empty() {
            sync_state = "00".to_string();
        }
        self.contact_registered_notifications_sync_state =
            match sync_state.as_bytes()[0].wrapping_sub(b'0') {
                0 => SyncState::NotSynced,
                1 => SyncState::Pending,
                _ => SyncState::Completed,
            };
        if self.contact_registered_notifications_sync_state != SyncState::Completed
            || sync_state.as_bytes()[1]
                != u8::from(self.disable_contact_registered_notifications) + b'0'
        {
            self.run_contact_registered_notifications_sync(env);
        }

        self.current_notification_id = NotificationId::new(parse_pmc_i32(
            &env.pmc_get(NOTIFICATION_ID_CURRENT_KEY),
            NOTIFICATION_ID_CURRENT_KEY,
        ));
        self.current_notification_group_id = NotificationGroupId::new(parse_pmc_i32(
            &env.pmc_get(NOTIFICATION_GROUP_ID_CURRENT_KEY),
            NOTIFICATION_GROUP_ID_CURRENT_KEY,
        ));

        self.on_notification_group_count_max_changed(env, false);
        self.on_notification_group_size_max_changed(env);

        self.on_online_cloud_timeout_changed(env);
        self.on_notification_cloud_delay_changed(env);
        self.on_notification_default_delay_changed(env);

        self.last_loaded_notification_group_key
            .last_notification_date = i32::MAX;
        if self.max_notification_group_count != 0 {
            let mut loaded_groups = 0;
            let needed_groups = self.max_notification_group_count as i32;
            loop {
                loaded_groups +=
                    self.load_message_notification_groups_from_database(env, needed_groups, false);
                if loaded_groups >= needed_groups
                    || self
                        .last_loaded_notification_group_key
                        .last_notification_date
                        == 0
                {
                    break;
                }
            }
        }

        let call_notification_group_ids_string = env.pmc_get(NOTIFICATION_CALL_GROUP_IDS_KEY);
        if !call_notification_group_ids_string.is_empty() {
            let call_notification_group_ids: Vec<NotificationGroupId> =
                call_notification_group_ids_string
                    .split(',')
                    .map(|part| NotificationGroupId::new(part.parse::<i32>().unwrap_or(0)))
                    .collect();
            for &group_id in &call_notification_group_ids {
                if !group_id.is_valid() {
                    continue;
                }
                if group_id.get() > self.current_notification_group_id.get() {
                    tdutils::log_error!(
                        "Fix current notification group identifier from {} to {}",
                        self.current_notification_group_id,
                        group_id
                    );
                    self.current_notification_group_id = group_id;
                    env.pmc_set(
                        NOTIFICATION_GROUP_ID_CURRENT_KEY,
                        &self.current_notification_group_id.get().to_string(),
                    );
                }
                if self.get_group_force(env, group_id, true).is_some() {
                    tdutils::log_error!("Have a loaded call notification group {}", group_id);
                } else {
                    self.call_notification_group_ids.push(group_id);
                    self.available_call_notification_group_ids.insert(group_id);
                }
            }
        }

        let notification_announcement_ids_string = env.pmc_get(NOTIFICATION_ANNOUNCEMENT_IDS_KEY);
        if !notification_announcement_ids_string.is_empty() {
            let stored_ids: Vec<i32> = notification_announcement_ids_string
                .split(',')
                .map(|part| part.parse::<i32>().unwrap_or(0))
                .collect();
            assert!(stored_ids.len().is_multiple_of(2));
            let mut is_changed = false;
            let min_date = env.unix_time() - ANNOUNCEMENT_ID_CACHE_TIME;
            for pair in stored_ids.chunks(2) {
                let announcement_id = pair[0];
                let date = pair[1];
                if date < min_date || announcement_id == 0 {
                    is_changed = true;
                    continue;
                }
                self.announcement_id_date
                    .entry(announcement_id)
                    .or_insert(date);
            }
            if is_changed {
                self.save_announcement_ids(env);
            }
        }

        // StateManager::add_callback (cpp:300-314) — the online callback
        // flushes pending notifications; dropped until the StateManager
        // lands, `flush_all_pending_notifications` is the flip target.

        self.is_inited = true;
        self.try_send_update_active_notifications(env);
    }

    /// `NotificationManager::save_announcement_ids` (cpp:316-338).
    pub fn save_announcement_ids(&mut self, env: &mut impl NotificationManagerEnv) {
        let min_date = env.unix_time() - ANNOUNCEMENT_ID_CACHE_TIME;
        let mut stored_ids = Vec::new();
        for (&announcement_id, &date) in &self.announcement_id_date {
            if date < min_date {
                continue;
            }
            stored_ids.push(announcement_id);
            stored_ids.push(date);
        }

        if stored_ids.is_empty() {
            env.pmc_erase(NOTIFICATION_ANNOUNCEMENT_IDS_KEY);
            return;
        }

        let string = stored_ids
            .iter()
            .map(i32::to_string)
            .collect::<Vec<String>>()
            .join(",");
        env.pmc_set(NOTIFICATION_ANNOUNCEMENT_IDS_KEY, &string);
    }

    /// `NotificationManager::get_update_active_notifications` (cpp:340-369).
    pub fn get_update_active_notifications(
        &self,
        env: &impl NotificationManagerEnv,
    ) -> td_api::updateActiveNotifications {
        let mut needed_groups = self.max_notification_group_count;
        let mut groups = Vec::new();
        for (&group_key, group) in &self.groups {
            if needed_groups == 0 || group_key.last_notification_date == 0 {
                break;
            }
            needed_groups -= 1;

            let mut notifications = Vec::new();
            for notification in group.notifications.iter().rev() {
                let notification_object =
                    get_notification_object(env, group_key.dialog_id, notification);
                if notification_object.r#type.is_some() {
                    notifications.push(notification_object);
                }
                if notifications.len() == self.max_notification_group_size {
                    break;
                }
            }
            if !notifications.is_empty() {
                notifications.reverse();
                groups.push(td_api::NotificationGroup {
                    id: group_key.group_id.get(),
                    r#type: Some(group.r#type.get_notification_group_type_object()),
                    chat_id: env.get_chat_id_object(group_key.dialog_id),
                    total_count: group.total_count,
                    notifications,
                });
            }
        }

        td_api::updateActiveNotifications { groups }
    }

    /// `NotificationManager::get_update_have_pending_notifications` (cpp:4149-4153).
    pub fn get_update_have_pending_notifications(&self) -> td_api::updateHavePendingNotifications {
        td_api::updateHavePendingNotifications {
            have_delayed_notifications: self.delayed_notification_update_count != 0,
            have_unreceived_notifications: self.unreceived_notification_update_count != 0,
        }
    }

    /// `NotificationManager::send_update_have_pending_notifications` (cpp:4155-4163).
    pub fn send_update_have_pending_notifications(&self, env: &mut impl NotificationManagerEnv) {
        if self.is_destroyed || !self.is_inited || !self.is_binlog_processed {
            return;
        }

        let update = self.get_update_have_pending_notifications();
        env.send_update(td_api::Update::updateHavePendingNotifications(update));
    }

    /// `NotificationManager::try_send_update_active_notifications` (cpp:4193-4208).
    pub fn try_send_update_active_notifications(&mut self, env: &mut impl NotificationManagerEnv) {
        if self.max_notification_group_count == 0 {
            return;
        }
        if !self.is_binlog_processed || !self.is_inited {
            return;
        }

        let update = self.get_update_active_notifications(env);
        env.send_update(td_api::Update::updateActiveNotifications(update));

        while !self.push_notification_promises.is_empty() {
            let notification_id = *self
                .push_notification_promises
                .keys()
                .next()
                .expect("non-empty");
            self.on_notification_processed(notification_id);
        }
    }

    /// `NotificationManager::get_current_state` (cpp:4100-4108).
    pub fn get_current_state(
        &self,
        env: &impl NotificationManagerEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        if env.is_disabled() || self.max_notification_group_count == 0 || self.is_destroyed {
            return;
        }

        updates.push(td_api::Update::updateActiveNotifications(
            self.get_update_active_notifications(env),
        ));
        updates.push(td_api::Update::updateHavePendingNotifications(
            self.get_update_have_pending_notifications(),
        ));
    }

    /// `NotificationManager::flush_all_notifications` (cpp:4109-4112).
    pub fn flush_all_notifications(&mut self, env: &mut impl NotificationManagerEnv) {
        self.flush_all_pending_notifications(env);
        self.flush_all_pending_updates(env, true, "flush_all_notifications");
    }

    /// `NotificationManager::destroy_all_notifications` (cpp:4114-4147).
    pub fn destroy_all_notifications(&mut self, env: &mut impl NotificationManagerEnv) {
        if self.is_destroyed {
            return;
        }
        self.is_being_destroyed = true;

        let mut to_remove = Vec::new();
        for (cur_pos, &group_key) in self.groups.keys().enumerate() {
            if cur_pos >= self.max_notification_group_count {
                break;
            }

            if group_key.last_notification_date == 0 {
                break;
            }

            to_remove.push(group_key);
        }
        for group_key in to_remove {
            let group = self
                .groups
                .get(&group_key)
                .cloned()
                .expect("the group exists");
            self.send_remove_group_update(env, group_key, &group, Vec::new());
        }

        self.flush_all_pending_updates(env, true, "destroy_all_notifications");
        if self.delayed_notification_update_count != 0 {
            let diff = -self.delayed_notification_update_count;
            self.on_delayed_notification_update_count_changed(
                env,
                diff,
                0,
                "destroy_all_notifications",
            );
        }
        if self.unreceived_notification_update_count != 0 {
            let diff = -self.unreceived_notification_update_count;
            self.on_unreceived_notification_update_count_changed(
                env,
                diff,
                0,
                "destroy_all_notifications",
            );
        }

        while !self.push_notification_promises.is_empty() {
            let notification_id = *self
                .push_notification_promises
                .keys()
                .next()
                .expect("non-empty");
            self.on_notification_processed(notification_id);
        }

        self.is_destroyed = true;
    }

    /// `NotificationManager::on_notification_processed` (cpp:1703-1711).
    pub fn on_notification_processed(&mut self, notification_id: NotificationId) {
        let Some(promises) = self.push_notification_promises.remove(&notification_id) else {
            return;
        };

        for mut promise in promises {
            promise.set_value(());
        }
    }

    /// `NotificationManager::on_notification_removed` (cpp:1713-1742).
    pub fn on_notification_removed(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        notification_id: NotificationId,
    ) {
        let Some(add_event_id) = self
            .temporary_notification_log_event_ids
            .remove(&notification_id)
        else {
            return;
        };

        if let Some(edit_event_id) = self
            .temporary_edit_notification_log_event_ids
            .remove(&notification_id)
            && !self.is_being_destroyed
        {
            env.binlog_erase(edit_event_id);
        }

        if !self.is_being_destroyed {
            env.binlog_erase(add_event_id);
        }

        let object_id = self
            .temporary_notification_object_ids
            .get(&notification_id)
            .copied()
            .unwrap_or_default();
        let erased_notification_count = self.temporary_notifications.remove(&object_id).is_some();
        let erased_object_id_count = self
            .temporary_notification_object_ids
            .remove(&notification_id)
            .is_some();
        assert!(erased_notification_count);
        assert!(erased_object_id_count);

        self.on_notification_processed(notification_id);
    }

    /// `NotificationManager::on_delayed_notification_update_count_changed`
    /// (cpp:4165-4177).
    pub fn on_delayed_notification_update_count_changed(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        diff: i32,
        _notification_group_id: i32,
        _source: &str,
    ) {
        let had_delayed = self.delayed_notification_update_count != 0;
        self.delayed_notification_update_count += diff;
        assert!(self.delayed_notification_update_count >= 0);
        let have_delayed = self.delayed_notification_update_count != 0;
        if had_delayed != have_delayed {
            self.send_update_have_pending_notifications(env);
        }
    }

    /// `NotificationManager::on_unreceived_notification_update_count_changed`
    /// (cpp:4179-4191).
    pub fn on_unreceived_notification_update_count_changed(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        diff: i32,
        _notification_group_id: i32,
        _source: &str,
    ) {
        let had_unreceived = self.unreceived_notification_update_count != 0;
        self.unreceived_notification_update_count += diff;
        assert!(self.unreceived_notification_update_count >= 0);
        let have_unreceived = self.unreceived_notification_update_count != 0;
        if had_unreceived != have_unreceived {
            self.send_update_have_pending_notifications(env);
        }
    }

    /// `NotificationManager::add_group` (cpp:375-385).
    pub fn add_group(
        &mut self,
        group_key: NotificationGroupKey,
        group: NotificationGroup,
        _source: &str,
    ) {
        if group.notifications.is_empty() {
            assert!(
                group_key.last_notification_date == 0,
                "Trying to add empty {group_key}"
            );
        }
        let is_inserted = self
            .group_keys
            .insert(group_key.group_id, group_key)
            .is_none();
        assert!(is_inserted);
        let is_inserted = self.groups.insert(group_key, group).is_none();
        assert!(is_inserted);
    }

    /// `NotificationManager::get_group` (cpp:386-392).
    pub fn get_group(&self, group_id: NotificationGroupId) -> Option<NotificationGroupKey> {
        self.group_keys.get(&group_id).copied()
    }

    /// `NotificationManager::load_group_force` (cpp:394-401).
    pub fn load_group_force(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_id: NotificationGroupId,
    ) {
        if env.is_disabled() || self.max_notification_group_count == 0 {
            return;
        }

        let group_key = self.get_group_force(env, group_id, true);
        assert!(group_key.is_some());
    }

    /// `NotificationManager::have_group_force` (cpp:403-409).
    pub fn have_group_force(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_id: NotificationGroupId,
    ) -> bool {
        if env.is_disabled() {
            return false;
        }

        self.call_notification_group_ids.contains(&group_id)
            || self.get_group_force(env, group_id, true).is_some()
    }

    /// `NotificationManager::get_group_force` (cpp:411-465) — returns the
    /// group key after ensuring the group is loaded; `None` mirrors the
    /// C++ `groups_.end()`.
    pub fn get_group_force(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_id: NotificationGroupId,
        send_update: bool,
    ) -> Option<NotificationGroupKey> {
        if let Some(group_key) = self.get_group(group_id) {
            return Some(group_key);
        }

        if self.call_notification_group_ids.contains(&group_id) {
            return None;
        }

        let mut message_group = env.get_message_notification_group_force(group_id)?;
        if !message_group.dialog_id.is_valid() {
            return None;
        }

        let mut group_key = NotificationGroupKey::new(group_id, message_group.dialog_id, 0);
        for notification in &message_group.notifications {
            if notification.date > group_key.last_notification_date {
                group_key.last_notification_date = notification.date;
            }
            if notification.notification_id.get() > self.current_notification_id.get() {
                tdutils::log_error!(
                    "Fix current notification identifier from {} to {}",
                    self.current_notification_id,
                    notification.notification_id
                );
                self.current_notification_id = notification.notification_id;
                env.pmc_set(
                    NOTIFICATION_ID_CURRENT_KEY,
                    &self.current_notification_id.get().to_string(),
                );
            }
        }
        if group_id.get() > self.current_notification_group_id.get() {
            tdutils::log_error!(
                "Fix current notification group identifier from {} to {}",
                self.current_notification_group_id,
                group_id
            );
            self.current_notification_group_id = group_id;
            env.pmc_set(
                NOTIFICATION_GROUP_ID_CURRENT_KEY,
                &self.current_notification_group_id.get().to_string(),
            );
        }

        let group = NotificationGroup {
            r#type: message_group.r#type,
            total_count: message_group.total_count,
            notifications: std::mem::take(&mut message_group.notifications),
            ..NotificationGroup::default()
        };

        if send_update && group_key.last_notification_date != 0 {
            let last_group_key = self.get_last_updated_group_key();
            if group_key < last_group_key {
                if last_group_key.last_notification_date != 0 {
                    let last_group = self
                        .groups
                        .get(&last_group_key)
                        .cloned()
                        .expect("the group exists");
                    self.send_remove_group_update(env, last_group_key, &last_group, Vec::new());
                }
                self.send_add_group_update(env, group_key, &group, "get_group_force");
            }
        }
        self.add_group(group_key, group, "get_group_force");
        Some(group_key)
    }

    /// `NotificationManager::delete_group` (cpp:467-471).
    pub fn delete_group(&mut self, group_key: NotificationGroupKey) {
        let erased_count = self.group_keys.remove(&group_key.group_id).is_some();
        assert!(erased_count);
        let erased_count = self.groups.remove(&group_key).is_some();
        assert!(erased_count);
    }

    /// `NotificationManager::get_last_updated_group_key` (cpp:828-839) —
    /// the first group key at or past the visible `max_notification_group_count_`.
    pub fn get_last_updated_group_key(&self) -> NotificationGroupKey {
        let mut left = self.max_notification_group_count;
        let mut it = self.groups.iter();
        while left > 1 {
            if it.next().is_none() {
                break;
            }
            left -= 1;
        }
        match it.next() {
            Some((&group_key, _)) => group_key,
            None => NotificationGroupKey::default(),
        }
    }

    /// `NotificationManager::get_max_notification_group_size` (cpp:752-754).
    pub fn get_max_notification_group_size(&self) -> usize {
        self.max_notification_group_size
    }

    /// `NotificationManager::get_max_notification_id` (cpp:756-758).
    pub fn get_max_notification_id(&self) -> NotificationId {
        self.current_notification_id
    }

    /// `NotificationManager::get_next_notification_id` (cpp:760-772).
    pub fn get_next_notification_id(
        &mut self,
        env: &mut impl NotificationManagerEnv,
    ) -> NotificationId {
        if env.is_disabled() {
            return NotificationId::default();
        }
        if self.current_notification_id.get() == i32::MAX {
            tdutils::log_error!("Notification identifier overflowed");
            return NotificationId::default();
        }

        self.current_notification_id = NotificationId::new(self.current_notification_id.get() + 1);
        env.pmc_set(
            NOTIFICATION_ID_CURRENT_KEY,
            &self.current_notification_id.get().to_string(),
        );
        self.current_notification_id
    }

    /// `NotificationManager::get_next_notification_group_id` (cpp:774-786).
    pub fn get_next_notification_group_id(
        &mut self,
        env: &mut impl NotificationManagerEnv,
    ) -> NotificationGroupId {
        if env.is_disabled() {
            return NotificationGroupId::default();
        }
        if self.current_notification_group_id.get() == i32::MAX {
            tdutils::log_error!("Notification group identifier overflowed");
            return NotificationGroupId::default();
        }

        self.current_notification_group_id =
            NotificationGroupId::new(self.current_notification_group_id.get() + 1);
        env.pmc_set(
            NOTIFICATION_GROUP_ID_CURRENT_KEY,
            &self.current_notification_group_id.get().to_string(),
        );
        self.current_notification_group_id
    }

    /// `NotificationManager::try_reuse_notification_group_id` (cpp:788-826).
    pub fn try_reuse_notification_group_id(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_id: NotificationGroupId,
    ) {
        if env.is_disabled() {
            return;
        }
        if !group_id.is_valid() {
            return;
        }

        if group_id != self.current_notification_group_id {
            // may be implemented in the future
            return;
        }

        if let Some(group_key) = self.get_group(group_id) {
            let group = self.groups.get(&group_key).expect("the group exists");
            assert!(
                group_key.last_notification_date == 0 && group.total_count == 0,
                "{} {} {} {} {} {} {}",
                self.running_get_difference,
                self.delayed_notification_update_count,
                self.unreceived_notification_update_count,
                self.pending_updates
                    .get(&group_id.get())
                    .map_or(0, Vec::len),
                group_key,
                group.total_count,
                group.is_being_loaded_from_database
            );
            assert!(group.notifications.is_empty());
            assert!(group.pending_notifications.is_empty());
            assert!(!group.is_being_loaded_from_database);
            self.delete_group(group_key);

            assert!(!self.running_get_chat_difference.contains(&group_id.get()));

            if !env.close_flag() {
                self.flush_pending_notifications_timeout
                    .remove(&(group_id.get() as i64));
                self.flush_pending_updates_timeout.remove(&group_id.get());
            }
            if self.pending_updates.remove(&group_id.get()).is_some() {
                let diff = -1;
                self.on_delayed_notification_update_count_changed(
                    env,
                    diff,
                    group_id.get(),
                    "try_reuse_notification_group_id",
                );
            }
        }

        self.current_notification_group_id =
            NotificationGroupId::new(self.current_notification_group_id.get() - 1);
        env.pmc_set(
            NOTIFICATION_GROUP_ID_CURRENT_KEY,
            &self.current_notification_group_id.get().to_string(),
        );
    }

    /// `NotificationManager::on_notification_group_count_max_changed`
    /// (cpp:2473-2535).
    pub fn on_notification_group_count_max_changed(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        send_updates: bool,
    ) {
        if env.is_disabled() {
            return;
        }

        let new_max_notification_group_count = env.get_option_integer(
            "notification_group_count_max",
            DEFAULT_GROUP_COUNT_MAX as i64,
        ) as i32;
        assert!(
            (MIN_NOTIFICATION_GROUP_COUNT_MAX..=MAX_NOTIFICATION_GROUP_COUNT_MAX)
                .contains(&new_max_notification_group_count)
        );

        let new_max_notification_group_count = new_max_notification_group_count as usize;
        if new_max_notification_group_count == self.max_notification_group_count {
            return;
        }

        let is_increased = new_max_notification_group_count > self.max_notification_group_count;
        if send_updates {
            self.flush_all_notifications(env);

            let min_group_count = self
                .max_notification_group_count
                .min(new_max_notification_group_count);
            let max_group_count = self
                .max_notification_group_count
                .max(new_max_notification_group_count);
            let mut keys = Vec::new();
            for (cur_pos, (&group_key, group)) in self.groups.iter().enumerate() {
                if cur_pos >= max_group_count {
                    break;
                }
                if cur_pos < min_group_count {
                    continue;
                }

                assert!(group.pending_notifications.is_empty());
                assert!(!self.pending_updates.contains_key(&group_key.group_id.get()));

                if group_key.last_notification_date == 0 {
                    break;
                }

                keys.push((group_key, is_increased));
            }
            for (group_key, is_increased) in keys {
                let group = self
                    .groups
                    .get(&group_key)
                    .cloned()
                    .expect("the group exists");
                if is_increased {
                    self.send_add_group_update(
                        env,
                        group_key,
                        &group,
                        "on_notification_group_count_max_changed",
                    );
                } else {
                    self.send_remove_group_update(env, group_key, &group, Vec::new());
                }
            }

            self.flush_all_pending_updates(env, true, "on_notification_group_size_max_changed end");

            if new_max_notification_group_count == 0 {
                self.last_loaded_notification_group_key = NotificationGroupKey::default();
                self.last_loaded_notification_group_key
                    .last_notification_date = i32::MAX;
                assert!(self.pending_updates.is_empty());
                self.groups.clear();
                self.group_keys.clear();
            }
        }

        self.max_notification_group_count = new_max_notification_group_count;
        if is_increased
            && self.last_loaded_notification_group_key < self.get_last_updated_group_key()
        {
            let limit = new_max_notification_group_count.max(5) as i32;
            self.load_message_notification_groups_from_database(env, limit, true);
        }
    }

    /// `NotificationManager::on_notification_group_size_max_changed`
    /// (cpp:2537-2619).
    pub fn on_notification_group_size_max_changed(
        &mut self,
        env: &mut impl NotificationManagerEnv,
    ) {
        if env.is_disabled() {
            return;
        }

        let new_max_notification_group_size = env
            .get_option_integer("notification_group_size_max", DEFAULT_GROUP_SIZE_MAX as i64)
            as i32;
        assert!(
            (MIN_NOTIFICATION_GROUP_SIZE_MAX..=MAX_NOTIFICATION_GROUP_SIZE_MAX)
                .contains(&new_max_notification_group_size)
        );

        let new_max_notification_group_size = new_max_notification_group_size as usize;
        if new_max_notification_group_size == self.max_notification_group_size {
            return;
        }

        let new_keep_notification_group_size = new_max_notification_group_size
            + new_max_notification_group_size.clamp(EXTRA_GROUP_SIZE / 2, EXTRA_GROUP_SIZE);

        if self.max_notification_group_size != 0 {
            self.flush_all_notifications(env);

            let mut left = self.max_notification_group_count;
            let mut updates_to_send = Vec::new();
            for (&group_key, group) in &self.groups {
                if left == 0 {
                    break;
                }
                left -= 1;
                assert!(group.pending_notifications.is_empty());
                assert!(!self.pending_updates.contains_key(&group_key.group_id.get()));

                if group_key.last_notification_date == 0 {
                    break;
                }

                let mut added_notifications: Vec<td_api::Notification> = Vec::new();
                let mut removed_notification_ids: Vec<i32> = Vec::new();
                let notification_count = group.notifications.len();
                if new_max_notification_group_size < self.max_notification_group_size {
                    if notification_count <= new_max_notification_group_size {
                        continue;
                    }
                    let begin = notification_count
                        - notification_count.min(self.max_notification_group_size);
                    let end = notification_count - new_max_notification_group_size;
                    for notification in &group.notifications[begin..end] {
                        removed_notification_ids.push(notification.notification_id.get());
                    }
                    assert!(!removed_notification_ids.is_empty());
                } else {
                    if new_max_notification_group_size > notification_count {
                        // [blocked: M.13.2] — load_notifications_from_database
                        // has no message-db layer yet.
                    }
                    if notification_count <= self.max_notification_group_size {
                        continue;
                    }
                    let begin = notification_count
                        - notification_count.min(new_max_notification_group_size);
                    let end = notification_count - self.max_notification_group_size;
                    for notification in &group.notifications[begin..end] {
                        let notification_object =
                            get_notification_object(env, group_key.dialog_id, notification);
                        if notification_object.r#type.is_some() {
                            added_notifications.push(notification_object);
                        }
                    }
                    if added_notifications.is_empty() {
                        continue;
                    }
                }
                if !self.is_destroyed {
                    updates_to_send.push(td_api::Update::updateNotificationGroup(
                        td_api::updateNotificationGroup {
                            notification_group_id: group_key.group_id.get(),
                            r#type: Some(group.r#type.get_notification_group_type_object()),
                            chat_id: env.get_chat_id_object(group_key.dialog_id),
                            notification_settings_chat_id: env
                                .get_chat_id_object(group_key.dialog_id),
                            notification_sound_id: 0,
                            total_count: group.total_count,
                            added_notifications,
                            removed_notification_ids,
                        },
                    ));
                }
            }
            for update in updates_to_send {
                env.send_update(update);
            }
        }

        self.max_notification_group_size = new_max_notification_group_size;
        self.keep_notification_group_size = new_keep_notification_group_size;
    }

    /// `NotificationManager::on_online_cloud_timeout_changed` (cpp:2621-2629).
    pub fn on_online_cloud_timeout_changed(&mut self, env: &impl NotificationManagerEnv) {
        if env.is_disabled() {
            return;
        }

        self.online_cloud_timeout_ms = env.get_option_integer(
            "online_cloud_timeout_ms",
            DEFAULT_ONLINE_CLOUD_TIMEOUT_MS as i64,
        ) as i32;
    }

    /// `NotificationManager::on_notification_cloud_delay_changed` (cpp:2631-2639).
    pub fn on_notification_cloud_delay_changed(&mut self, env: &impl NotificationManagerEnv) {
        if env.is_disabled() {
            return;
        }

        self.notification_cloud_delay_ms = env.get_option_integer(
            "notification_cloud_delay_ms",
            DEFAULT_ONLINE_CLOUD_DELAY_MS as i64,
        ) as i32;
    }

    /// `NotificationManager::on_notification_default_delay_changed` (cpp:2641-2649).
    pub fn on_notification_default_delay_changed(&mut self, env: &impl NotificationManagerEnv) {
        if env.is_disabled() {
            return;
        }

        self.notification_default_delay_ms = env.get_option_integer(
            "notification_default_delay_ms",
            DEFAULT_DEFAULT_DELAY_MS as i64,
        ) as i32;
    }

    /// `NotificationManager::on_disable_contact_registered_notifications_changed`
    /// (cpp:2651-2665).
    pub fn on_disable_contact_registered_notifications_changed(
        &mut self,
        env: &mut impl NotificationManagerEnv,
    ) {
        if env.is_disabled() {
            return;
        }

        let is_disabled = env.get_option_boolean("disable_contact_registered_notifications", false);
        if is_disabled == self.disable_contact_registered_notifications {
            return;
        }

        self.disable_contact_registered_notifications = is_disabled;
        if self.contact_registered_notifications_sync_state == SyncState::Completed {
            self.run_contact_registered_notifications_sync(env);
        }
    }

    /// `NotificationManager::set_contact_registered_notifications_sync_state`
    /// (cpp:2681-2691) — the pmc write is real; the query that drives
    /// the state machine rides the `NotificationNetKind` plane
    /// (net-drain 61).
    pub fn set_contact_registered_notifications_sync_state(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        new_state: SyncState,
    ) {
        if env.is_disabled() {
            return;
        }

        self.contact_registered_notifications_sync_state = new_state;
        let mut value = String::new();
        value.push(char::from(
            b'0' + match new_state {
                SyncState::NotSynced => 0,
                SyncState::Pending => 1,
                SyncState::Completed => 2,
            },
        ));
        value.push(char::from(
            b'0' + u8::from(self.disable_contact_registered_notifications),
        ));
        env.pmc_set(CONTACT_REGISTERED_SYNC_KEY, &value);
    }

    /// `run_contact_registered_notifications_sync`
    /// (cpp:2693-2714) — the net-drain 61 send leg: the
    /// `account.setContactSignUpNotification` query rides the
    /// `NotificationNetKind` plane; the answer walks
    /// `on_contact_registered_notifications_sync` (the promise lambda's
    /// send-time capture rides the kind).
    pub fn run_contact_registered_notifications_sync(
        &mut self,
        env: &mut impl NotificationManagerEnv,
    ) {
        if env.is_disabled() {
            return;
        }

        let is_disabled = self.disable_contact_registered_notifications;
        if self.contact_registered_notifications_sync_state == SyncState::NotSynced && !is_disabled
        {
            self.set_contact_registered_notifications_sync_state(env, SyncState::Completed);
            return;
        }
        if self.contact_registered_notifications_sync_state != SyncState::Pending {
            self.set_contact_registered_notifications_sync_state(env, SyncState::Pending);
        }

        tdutils::log_info!("Send SetContactSignUpNotificationQuery with {is_disabled}");
        self.send_notification_net_query(
            NotificationNetKind::SetContactSignUpNotification { is_disabled },
            tdmtproto::telegram_api::Function::account_setContactSignUpNotification(
                tdmtproto::telegram_api::account_setContactSignUpNotification {
                    silent: is_disabled,
                },
            ),
            "account.setContactSignUpNotification",
        );
    }

    /// `on_contact_registered_notifications_sync` (cpp:2716-2724) — the
    /// set-answer walk: a changed flag re-runs the sync, an ok marks
    /// Completed, an error resends forever.
    pub fn on_contact_registered_notifications_sync(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        is_disabled: bool,
        result: Result<(), Status>,
    ) {
        debug_assert_eq!(
            self.contact_registered_notifications_sync_state,
            SyncState::Pending,
            "the C++ CHECK(state == Pending)"
        );
        if is_disabled != self.disable_contact_registered_notifications {
            return self.run_contact_registered_notifications_sync(env);
        }
        match result {
            // everything is synchronized
            Ok(()) => {
                self.set_contact_registered_notifications_sync_state(env, SyncState::Completed)
            }
            // let's resend the query forever
            Err(_) => self.run_contact_registered_notifications_sync(env),
        }
    }

    /// `get_disable_contact_registered_notifications`
    /// (cpp:2729-2744) — the option getter half over the
    /// `GetContactSignUpNotificationQuery` family; the request parks
    /// FIFO until the Bool answer lands. The disabled band answers
    /// inline at the caller (the C++ `promise.set_value(Unit())`,
    /// cpp:2731-2733) — nothing parks.
    pub fn get_disable_contact_registered_notifications(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        request_id: u64,
    ) {
        if env.is_disabled() {
            return;
        }
        self.pending_contact_sign_up_get_options.push(request_id);
        self.send_notification_net_query(
            NotificationNetKind::GetContactSignUpNotification,
            tdmtproto::telegram_api::Function::account_getContactSignUpNotification(
                tdmtproto::telegram_api::account_getContactSignUpNotification {},
            ),
            "account.getContactSignUpNotification",
        );
    }

    /// `on_get_disable_contact_registered_notifications`
    /// (cpp:2667-2678) — the Bool answer walk: an unchanged flag or a
    /// closing client answers immediately, otherwise the flag flips and
    /// the option store write lands; the caller answers the parked
    /// request with the re-read value (the C++ wrap_promise,
    /// OptionManager.cpp:761-766).
    pub fn on_get_disable_contact_registered_notifications(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        is_disabled: bool,
    ) -> u64 {
        let Some(request_id) = self.pending_contact_sign_up_get_options.first().copied() else {
            return 0;
        };
        self.pending_contact_sign_up_get_options.remove(0);
        if env.close_flag() || self.disable_contact_registered_notifications == is_disabled {
            return request_id;
        }
        self.disable_contact_registered_notifications = is_disabled;

        if is_disabled {
            env.set_contact_option_boolean("disable_contact_registered_notifications", is_disabled);
        } else {
            env.set_contact_option_empty("disable_contact_registered_notifications");
        }
        request_id
    }

    /// The `td_->create_handler<…>()->send(...)` collapse (the
    /// `CallManager::send_call_net_query` shape) — `None` keeps the
    /// net-inert posture.
    fn send_notification_net_query(
        &self,
        kind: NotificationNetKind,
        function: tdmtproto::telegram_api::Function,
        what: &str,
    ) {
        match &self.net {
            Some(net) => net.dispatch(kind, function),
            None => tdutils::log_info!("Send {what} — [net-inert]"),
        }
    }

    /// `NotificationManager::add_update` (cpp:989-1006).
    pub fn add_update(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_id: i32,
        update: td_api::Update,
    ) {
        if !self.is_binlog_processed || !self.is_inited {
            return;
        }
        let is_first = self
            .pending_updates
            .get(&group_id)
            .is_none_or(Vec::is_empty);
        if is_first {
            self.on_delayed_notification_update_count_changed(env, 1, group_id, "add_update");
        }
        self.pending_updates
            .entry(group_id)
            .or_default()
            .push(Some(update));
        if !env.close_flag() {
            if !self.running_get_difference && !self.running_get_chat_difference.contains(&group_id)
            {
                self.add_flush_pending_updates_timeout(
                    env,
                    group_id,
                    MIN_UPDATE_DELAY_MS as f64 * 1e-3,
                );
            } else {
                self.set_flush_pending_updates_timeout(
                    env,
                    group_id,
                    MAX_UPDATE_DELAY_MS as f64 * 1e-3,
                );
            }
        }
    }

    /// `NotificationManager::add_update_notification_group` (cpp:1008-1025).
    pub fn add_update_notification_group(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        mut update: td_api::updateNotificationGroup,
    ) {
        let group_id = update.notification_group_id;
        if update.notification_settings_chat_id == 0 {
            update.notification_settings_chat_id = update.chat_id;
        }
        if !update.added_notifications.is_empty() && !update.removed_notification_ids.is_empty() {
            // just in case
            let removed = update.removed_notification_ids.clone();
            update.added_notifications.retain(|notification| {
                if removed.contains(&notification.id) {
                    tdutils::log_error!("Have the same notification as added and removed");
                    return false;
                }
                true
            });
        }
        self.add_update(
            env,
            group_id,
            td_api::Update::updateNotificationGroup(update),
        );
    }

    /// `NotificationManager::add_update_notification` (cpp:1027-1039).
    pub fn add_update_notification(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        notification_group_id: NotificationGroupId,
        dialog_id: DialogId,
        notification: &Notification,
    ) {
        let notification_object = get_notification_object(env, dialog_id, notification);
        if notification_object.r#type.is_none() {
            return;
        }

        self.add_update(
            env,
            notification_group_id.get(),
            td_api::Update::updateNotification(td_api::updateNotification {
                notification_group_id: notification_group_id.get(),
                notification: Some(notification_object),
            }),
        );
        if !notification.r#type.can_be_delayed() {
            self.force_flush_pending_updates(env, notification_group_id, "add_update_notification");
        }
    }

    /// `NotificationManager::flush_pending_updates` (cpp:1041-1385) — the
    /// per-group update diff: fold the queued updateNotification/
    /// updateNotificationGroup sequence into a minimal, correctly ordered
    /// final state.
    pub fn flush_pending_updates(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_id: i32,
        source: &str,
    ) {
        // no check for the close flag to flush pending updates even while closing
        let Some(mut updates) = self.pending_updates.remove(&group_id) else {
            return;
        };
        updates.retain(Option::is_some);

        if self.is_destroyed {
            return;
        }

        // if a notification was added, then deleted and then re-added we need
        // to keep the first addition (it can be with sound), the deletion
        // (the notification count must never exceed max_notification_group_size_)
        // and the second addition; the final state below implements that.

        // calculate the last state of all notifications
        let mut added_notification_ids: HashSet<i32> = HashSet::new();
        let mut edited_notification_ids: HashSet<i32> = HashSet::new();
        let mut removed_notification_ids: HashSet<i32> = HashSet::new();
        for update in updates.iter_mut() {
            let update = update.as_mut().expect("no nulls before the diff");
            match update {
                td_api::Update::updateNotificationGroup(group_update) => {
                    for notification in group_update.added_notifications.iter() {
                        let notification_id = notification.id;
                        assert!(notification_id != 0);
                        assert!(
                            added_notification_ids.insert(notification_id),
                            "there must be no additions after addition"
                        );
                        assert!(
                            !edited_notification_ids.contains(&notification_id),
                            "there must be no additions after edit"
                        );
                        removed_notification_ids.remove(&notification_id);
                    }
                    for notification_id in group_update.removed_notification_ids.iter_mut() {
                        let id = *notification_id;
                        assert!(id != 0);
                        added_notification_ids.remove(&id);
                        edited_notification_ids.remove(&id);
                        if !removed_notification_ids.insert(id) {
                            // the notification was already deleted at the time
                            // of addition and get_notification_type_object was
                            // None
                            *notification_id = 0;
                        }
                    }
                    group_update
                        .removed_notification_ids
                        .retain(|notification_id| *notification_id != 0);
                }
                td_api::Update::updateNotification(notification_update) => {
                    let notification_id = notification_update
                        .notification
                        .as_ref()
                        .expect("no nulls before the diff")
                        .id;
                    assert!(notification_id != 0);
                    assert!(
                        !removed_notification_ids.contains(&notification_id),
                        "there must be no edits of deleted notifications"
                    );
                    added_notification_ids.remove(&notification_id);
                    edited_notification_ids.insert(notification_id);
                }
                _ => unreachable!("a pending notification update has a known constructor"),
            }
        }

        // keep only additions of notifications from added/edited sets and all
        // edits of notifications from edited set; deletions of unkept
        // notifications move to the first updateNotificationGroup — see the
        // C++ block comment for the full invariant
        let group_key = self
            .group_keys
            .get(&NotificationGroupId::new(group_id))
            .copied()
            .unwrap_or_default();
        let is_hidden =
            group_key.last_notification_date == 0 || self.get_last_updated_group_key() < group_key;
        let mut is_changed = true;
        while is_changed {
            is_changed = false;

            let mut cur_pos = 0usize;
            let mut first_add_notification_pos: HashMap<i32, usize> = HashMap::new();
            let mut first_edit_notification_pos: HashMap<i32, usize> = HashMap::new();
            let mut can_be_deleted_notification_ids: HashSet<i32> = HashSet::new();
            let mut moved_deleted_notification_ids: Vec<i32> = Vec::new();
            let mut first_notification_group_pos = 0usize;

            let mut update_index = 0;
            while update_index < updates.len() {
                let mut update = updates[update_index]
                    .take()
                    .expect("no nulls at pass start");
                cur_pos += 1;
                let mut drop_update = false;
                match &mut update {
                    td_api::Update::updateNotificationGroup(group_update) => {
                        let added_len = group_update.added_notifications.len();
                        let mut added_alive = vec![true; added_len];
                        for (slot_index, notification) in
                            group_update.added_notifications.iter_mut().enumerate()
                        {
                            let notification_id = notification.id;
                            assert!(notification_id != 0);
                            let is_needed = added_notification_ids.contains(&notification_id)
                                || edited_notification_ids.contains(&notification_id);
                            if !is_needed {
                                can_be_deleted_notification_ids.insert(notification_id);
                                added_alive[slot_index] = false;
                                is_changed = true;
                                continue;
                            }

                            if let Some(edit_pos) =
                                first_edit_notification_pos.get(&notification_id).copied()
                            {
                                assert!(edit_pos < cur_pos);
                                let previous_slot = &mut updates[edit_pos - 1];
                                let previous = previous_slot.as_mut().expect("kept this pass");
                                let previous = match previous {
                                    td_api::Update::updateNotification(previous) => previous,
                                    _ => unreachable!(),
                                };
                                assert!(
                                    previous.notification.as_ref().expect("kept").id
                                        == notification_id
                                );
                                previous.notification.as_mut().expect("kept").r#type =
                                    notification.r#type.take();
                                is_changed = true;
                                added_alive[slot_index] = false;
                                continue;
                            }
                            if let Some(add_pos) =
                                first_add_notification_pos.get(&notification_id).copied()
                            {
                                assert!(add_pos < cur_pos);
                                let previous_slot = &mut updates[add_pos - 1];
                                let previous = previous_slot.as_mut().expect("kept this pass");
                                let previous = match previous {
                                    td_api::Update::updateNotificationGroup(previous) => previous,
                                    _ => unreachable!(),
                                };
                                let mut is_found = false;
                                for prev_notification in previous.added_notifications.iter_mut() {
                                    if prev_notification.id == notification_id {
                                        prev_notification.r#type = notification.r#type.take();
                                        is_found = true;
                                        break;
                                    }
                                }
                                assert!(is_found);
                                is_changed = true;
                                added_alive[slot_index] = false;
                                continue;
                            }

                            // it is a first addition/edit of a needed notification
                            first_add_notification_pos.insert(notification_id, cur_pos);
                        }
                        // the C++ remove_if of the nulled additions
                        group_update.added_notifications = group_update
                            .added_notifications
                            .drain(..)
                            .zip(added_alive)
                            .filter_map(|(notification, alive)| alive.then_some(notification))
                            .collect();
                        if group_update.added_notifications.is_empty()
                            && group_update.notification_sound_id != 0
                        {
                            group_update.notification_sound_id = 0;
                            is_changed = true;
                        }

                        for notification_id_slot in group_update.removed_notification_ids.iter_mut()
                        {
                            let notification_id = *notification_id_slot;
                            let is_needed = added_notification_ids.contains(&notification_id)
                                || edited_notification_ids.contains(&notification_id);
                            if can_be_deleted_notification_ids.contains(&notification_id) {
                                assert!(!is_needed);
                                *notification_id_slot = 0;
                                is_changed = true;
                                continue;
                            }
                            if !is_needed {
                                if first_notification_group_pos != 0 {
                                    moved_deleted_notification_ids.push(notification_id);
                                    *notification_id_slot = 0;
                                    is_changed = true;
                                }
                                continue;
                            }

                            if first_add_notification_pos.contains_key(&notification_id)
                                || first_edit_notification_pos.contains_key(&notification_id)
                            {
                                // the notification will be re-added, and the
                                // addition merges with the previous update
                                *notification_id_slot = 0;
                                is_changed = true;
                                continue;
                            }

                            // keep the deletion, otherwise there would be 2
                            // consequent additions
                        }
                        group_update
                            .removed_notification_ids
                            .retain(|notification_id| *notification_id != 0);

                        if group_update.removed_notification_ids.is_empty()
                            && group_update.added_notifications.is_empty()
                        {
                            for i in (1..cur_pos).rev() {
                                if let Some(td_api::Update::updateNotificationGroup(previous)) =
                                    updates[i - 1].as_mut()
                                {
                                    previous.r#type = group_update.r#type.take();
                                    previous.total_count = group_update.total_count;
                                    is_changed = true;
                                    drop_update = true;
                                    break;
                                }
                            }
                            if !drop_update && cur_pos == 1 {
                                let is_empty_group = added_notification_ids.is_empty()
                                    && edited_notification_ids.is_empty()
                                    && group_update.total_count == 0;
                                if updates.len() > 1 || (is_hidden && !is_empty_group) {
                                    assert!(moved_deleted_notification_ids.is_empty());
                                    is_changed = true;
                                    drop_update = true;
                                }
                            }
                        }

                        if first_notification_group_pos == 0 && !drop_update {
                            first_notification_group_pos = cur_pos;
                        }
                    }
                    td_api::Update::updateNotification(notification_update) => {
                        let notification_id = notification_update
                            .notification
                            .as_ref()
                            .expect("no nulls at pass start")
                            .id;
                        let is_needed = added_notification_ids.contains(&notification_id)
                            || edited_notification_ids.contains(&notification_id);
                        if !is_needed {
                            is_changed = true;
                            drop_update = true;
                        } else if let Some(edit_pos) =
                            first_edit_notification_pos.get(&notification_id).copied()
                        {
                            assert!(edit_pos < cur_pos);
                            let previous_slot = &mut updates[edit_pos - 1];
                            let previous = previous_slot.as_mut().expect("kept this pass");
                            let previous = match previous {
                                td_api::Update::updateNotification(previous) => previous,
                                _ => unreachable!(),
                            };
                            assert!(
                                previous.notification.as_ref().expect("kept").id == notification_id
                            );
                            previous.notification.as_mut().expect("kept").r#type =
                                notification_update
                                    .notification
                                    .as_mut()
                                    .expect("kept")
                                    .r#type
                                    .take();
                            is_changed = true;
                            drop_update = true;
                        } else if let Some(add_pos) =
                            first_add_notification_pos.get(&notification_id).copied()
                        {
                            assert!(add_pos < cur_pos);
                            let previous_slot = &mut updates[add_pos - 1];
                            let previous = previous_slot.as_mut().expect("kept this pass");
                            let previous = match previous {
                                td_api::Update::updateNotificationGroup(previous) => previous,
                                _ => unreachable!(),
                            };
                            let mut is_found = false;
                            for prev_notification in previous.added_notifications.iter_mut() {
                                if prev_notification.id == notification_id {
                                    prev_notification.r#type = notification_update
                                        .notification
                                        .as_mut()
                                        .expect("kept")
                                        .r#type
                                        .take();
                                    is_found = true;
                                    break;
                                }
                            }
                            assert!(is_found);
                            is_changed = true;
                            drop_update = true;
                        } else {
                            // it is a first addition/edit of a needed notification
                            first_edit_notification_pos.insert(notification_id, cur_pos);
                        }
                    }
                    _ => unreachable!("a pending notification update has a known constructor"),
                }
                if !drop_update {
                    updates[update_index] = Some(update);
                }
                update_index += 1;
            }
            if !moved_deleted_notification_ids.is_empty() {
                assert!(first_notification_group_pos != 0);
                let update_slot = &mut updates[first_notification_group_pos - 1];
                let update = update_slot.as_mut().expect("kept this pass");
                let group_update = match update {
                    td_api::Update::updateNotificationGroup(group_update) => group_update,
                    _ => unreachable!(),
                };
                group_update
                    .removed_notification_ids
                    .append(&mut moved_deleted_notification_ids);
                let old_size = group_update.removed_notification_ids.len();
                group_update.removed_notification_ids.sort_unstable();
                group_update.removed_notification_ids.dedup();
                assert!(old_size == group_update.removed_notification_ids.len());
            }

            updates.retain(Option::is_some);
            if updates.is_empty() {
                break;
            }

            fn has_common_notifications(
                notifications: &[td_api::Notification],
                notification_ids: &[i32],
            ) -> bool {
                notifications
                    .iter()
                    .any(|notification| notification_ids.contains(&notification.id))
            }

            let mut last_update_pos = 0usize;
            let mut i = 1usize;
            while i < updates.len() {
                let can_combine = {
                    let last_update = updates[last_update_pos].as_ref().expect("no nulls");
                    let cur_update = updates[i].as_ref().expect("no nulls");
                    match (last_update, cur_update) {
                        (
                            td_api::Update::updateNotificationGroup(last_group_update),
                            td_api::Update::updateNotificationGroup(group_update),
                        ) => {
                            (last_group_update.notification_settings_chat_id
                                == group_update.notification_settings_chat_id
                                || last_group_update.added_notifications.is_empty())
                                && !has_common_notifications(
                                    &last_group_update.added_notifications,
                                    &group_update.removed_notification_ids,
                                )
                                && !has_common_notifications(
                                    &group_update.added_notifications,
                                    &last_group_update.removed_notification_ids,
                                )
                                && last_group_update.notification_sound_id
                                    == group_update.notification_sound_id
                        }
                        _ => false,
                    }
                };
                if can_combine {
                    let (left, right) = updates.split_at_mut(i);
                    let last_slot = &mut left[last_update_pos];
                    let cur_slot = &mut right[0];
                    let (
                        td_api::Update::updateNotificationGroup(last_group_update),
                        td_api::Update::updateNotificationGroup(group_update),
                    ) = (
                        last_slot.as_mut().expect("no nulls"),
                        cur_slot.as_mut().expect("no nulls"),
                    )
                    else {
                        unreachable!()
                    };
                    assert_eq!(
                        last_group_update.notification_group_id,
                        group_update.notification_group_id
                    );
                    assert_eq!(last_group_update.chat_id, group_update.chat_id);
                    last_group_update.notification_settings_chat_id =
                        group_update.notification_settings_chat_id;
                    last_group_update.r#type = group_update.r#type.take();
                    last_group_update.total_count = group_update.total_count;
                    last_group_update
                        .added_notifications
                        .append(&mut group_update.added_notifications);
                    last_group_update
                        .removed_notification_ids
                        .append(&mut group_update.removed_notification_ids);
                    right[0] = None;
                    is_changed = true;
                    // the C++ `continue` re-enters the for loop, whose
                    // header advances `i`
                    i += 1;
                    continue;
                }
                last_update_pos += 1;
                if last_update_pos != i {
                    updates[last_update_pos] = updates[i].take();
                }
                i += 1;
            }
            updates.truncate(last_update_pos + 1);
        }

        for mut update in updates.drain(..).map(Option::unwrap) {
            if let td_api::Update::updateNotificationGroup(group_update) = &mut update {
                group_update
                    .added_notifications
                    .sort_by_key(|notification| notification.id);
                group_update.removed_notification_ids.sort_unstable();
            }
            env.send_update(update);
        }
        self.on_delayed_notification_update_count_changed(env, -1, group_id, source);

        let group_key = self
            .get_group_force(env, NotificationGroupId::new(group_id), false)
            .expect("the group exists after the flush");
        let group = self.groups.get(&group_key).expect("the group exists");
        let notification_ids: Vec<NotificationId> = group
            .notifications
            .iter()
            .map(|notification| notification.notification_id)
            .collect();
        for notification_id in notification_ids {
            self.on_notification_processed(notification_id);
        }
    }

    /// `NotificationManager::force_flush_pending_updates` (cpp:1386-1391).
    pub fn force_flush_pending_updates(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_id: NotificationGroupId,
        source: &str,
    ) {
        if !env.close_flag() {
            self.flush_pending_updates_timeout.remove(&group_id.get());
        }
        self.flush_pending_updates(env, group_id.get(), source);
    }

    /// `NotificationManager::flush_all_pending_updates` (cpp:1393-1418).
    pub fn flush_all_pending_updates(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        include_delayed_chats: bool,
        _source: &str,
    ) {
        if !include_delayed_chats && self.running_get_difference {
            return;
        }

        let mut ready_group_keys: Vec<NotificationGroupKey> = Vec::new();
        for &group_id in self.pending_updates.keys() {
            if include_delayed_chats || !self.running_get_chat_difference.contains(&group_id) {
                let group_key = self
                    .get_group(NotificationGroupId::new(group_id))
                    .expect("the group exists");
                ready_group_keys.push(group_key);
            }
        }

        // flush groups in reverse order to not exceed max_notification_group_count_
        ready_group_keys.sort_unstable();
        for group_key in ready_group_keys.into_iter().rev() {
            self.force_flush_pending_updates(env, group_key.group_id, "flush_all_pending_updates");
        }
        if include_delayed_chats {
            assert!(self.pending_updates.is_empty());
        }
    }

    /// `NotificationManager::do_flush_pending_notifications` (cpp:1420-1477).
    pub fn do_flush_pending_notifications(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_key: NotificationGroupKey,
        group: &mut NotificationGroup,
        pending_notifications: &mut Vec<PendingNotification>,
    ) -> bool {
        // no check for the close flag to flush pending notifications even while closing
        if pending_notifications.is_empty() {
            return false;
        }

        let old_notification_count = group.notifications.len();
        let shown_notification_count = old_notification_count.min(self.max_notification_group_size);

        let mut force_update = false;
        let mut added_notifications: Vec<td_api::Notification> = Vec::new();
        let first_settings_dialog_id = pending_notifications[0].settings_dialog_id;
        let first_ringtone_id = pending_notifications[0].ringtone_id;
        for pending_notification in pending_notifications.drain(..) {
            let notification = Notification::new(
                pending_notification.notification_id,
                pending_notification.date,
                pending_notification.disable_notification,
                pending_notification.r#type,
            );
            let notification_object =
                get_notification_object(env, group_key.dialog_id, &notification);
            assert!(notification_object.r#type.is_some());

            if !notification.r#type.can_be_delayed() {
                force_update = true;
            }
            added_notifications.push(notification_object);
            group.notifications.push(notification);
        }
        group.total_count += added_notifications.len() as i32;
        if added_notifications.len() > self.max_notification_group_size {
            let excess = added_notifications.len() - self.max_notification_group_size;
            added_notifications.drain(0..excess);
        }

        let mut removed_notification_ids: Vec<i32> = Vec::new();
        if shown_notification_count + added_notifications.len() > self.max_notification_group_size {
            let removed_notification_count = shown_notification_count + added_notifications.len()
                - self.max_notification_group_size;
            for i in 0..removed_notification_count {
                removed_notification_ids.push(
                    group.notifications[old_notification_count - shown_notification_count + i]
                        .notification_id
                        .get(),
                );
            }
        }

        if !added_notifications.is_empty() {
            self.add_update_notification_group(
                env,
                td_api::updateNotificationGroup {
                    notification_group_id: group_key.group_id.get(),
                    r#type: Some(group.r#type.get_notification_group_type_object()),
                    chat_id: env.get_chat_id_object(group_key.dialog_id),
                    notification_settings_chat_id: env.get_chat_id_object(first_settings_dialog_id),
                    notification_sound_id: first_ringtone_id,
                    total_count: group.total_count,
                    added_notifications,
                    removed_notification_ids,
                },
            );
        } else {
            assert!(removed_notification_ids.is_empty());
        }
        force_update
    }

    /// `NotificationManager::get_remove_group_update` (cpp:1479-1498).
    pub fn get_remove_group_update(
        &self,
        env: &impl NotificationManagerEnv,
        group_key: NotificationGroupKey,
        group: &NotificationGroup,
        mut removed_notification_ids: Vec<i32>,
    ) -> Option<td_api::updateNotificationGroup> {
        let total_size = group.notifications.len();
        assert!(removed_notification_ids.len() <= self.max_notification_group_size);
        let removed_size =
            total_size.min(self.max_notification_group_size - removed_notification_ids.len());
        for i in total_size - removed_size..total_size {
            removed_notification_ids.push(group.notifications[i].notification_id.get());
        }

        if removed_notification_ids.is_empty() {
            return None;
        }
        Some(td_api::updateNotificationGroup {
            notification_group_id: group_key.group_id.get(),
            r#type: Some(group.r#type.get_notification_group_type_object()),
            chat_id: env.get_chat_id_object(group_key.dialog_id),
            notification_settings_chat_id: env.get_chat_id_object(group_key.dialog_id),
            notification_sound_id: 0,
            total_count: group.total_count,
            added_notifications: Vec::new(),
            removed_notification_ids,
        })
    }

    /// `NotificationManager::send_remove_group_update` (cpp:1500-1508).
    pub fn send_remove_group_update(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_key: NotificationGroupKey,
        group: &NotificationGroup,
        removed_notification_ids: Vec<i32>,
    ) {
        let update = self.get_remove_group_update(env, group_key, group, removed_notification_ids);
        if let Some(update) = update {
            self.add_update_notification_group(env, update);
        }
    }

    /// `NotificationManager::send_add_group_update` (cpp:1510-1530).
    pub fn send_add_group_update(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_key: NotificationGroupKey,
        group: &NotificationGroup,
        _source: &str,
    ) {
        let total_size = group.notifications.len();
        let added_size = total_size.min(self.max_notification_group_size);
        let mut added_notifications: Vec<td_api::Notification> = Vec::new();
        for i in total_size - added_size..total_size {
            let notification_object =
                get_notification_object(env, group_key.dialog_id, &group.notifications[i]);
            if notification_object.r#type.is_some() {
                added_notifications.push(notification_object);
            }
        }

        if !added_notifications.is_empty() {
            self.add_update_notification_group(
                env,
                td_api::updateNotificationGroup {
                    notification_group_id: group_key.group_id.get(),
                    r#type: Some(group.r#type.get_notification_group_type_object()),
                    chat_id: env.get_chat_id_object(group_key.dialog_id),
                    notification_settings_chat_id: 0,
                    notification_sound_id: 0,
                    total_count: group.total_count,
                    added_notifications,
                    removed_notification_ids: Vec::new(),
                },
            );
        }
    }

    /// `NotificationManager::flush_pending_notifications` (cpp:1532-1631).
    pub fn flush_pending_notifications(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_id: NotificationGroupId,
    ) {
        let Some(group_key) = self.get_group(group_id) else {
            return;
        };

        let dialog_id = group_key.dialog_id;
        let group = self.groups.get_mut(&group_key).expect("the group exists");
        group.pending_notifications.retain(|pending_notification| {
            pending_notification
                .r#type
                .get_notification_type_object(env, dialog_id)
                .is_some()
        });
        if group.pending_notifications.is_empty() {
            return;
        }

        let mut group = self.groups.remove(&group_key).expect("the group exists");
        let erased_count = self.group_keys.remove(&group_key.group_id).is_some();
        assert!(erased_count);

        let mut final_group_key = group_key;
        for pending_notification in &group.pending_notifications {
            if pending_notification.date >= final_group_key.last_notification_date {
                final_group_key.last_notification_date = pending_notification.date;
            }
        }
        assert!(final_group_key.last_notification_date != 0);

        let last_group_key = self.get_last_updated_group_key();
        let was_updated = group_key.last_notification_date != 0 && group_key < last_group_key;
        let is_updated = final_group_key < last_group_key;
        let mut force_update = false;

        let mut removed_group_id = NotificationGroupId::default();
        if !is_updated {
            assert!(!was_updated);
            group.total_count += group.pending_notifications.len() as i32;
            for pending_notification in group.pending_notifications.drain(..) {
                group.notifications.push(Notification::new(
                    pending_notification.notification_id,
                    pending_notification.date,
                    pending_notification.disable_notification,
                    pending_notification.r#type,
                ));
            }
        } else {
            if !was_updated {
                if last_group_key.last_notification_date != 0 {
                    // need to remove the last notification group to not
                    // exceed max_notification_group_count_
                    removed_group_id = last_group_key.group_id;
                    let last_group = self
                        .groups
                        .get(&last_group_key)
                        .cloned()
                        .expect("the group exists");
                    self.send_remove_group_update(env, last_group_key, &last_group, Vec::new());
                }
                self.send_add_group_update(env, group_key, &group, "flush_pending_notifications");
            }

            let mut notification_settings_dialog_id = DialogId::default();
            let mut ringtone_id: i64 = -1;

            // split notifications by groups with common settings
            let mut grouped_notifications: Vec<PendingNotification> = Vec::new();
            for pending_notification in std::mem::take(&mut group.pending_notifications) {
                if notification_settings_dialog_id != pending_notification.settings_dialog_id
                    || ringtone_id != pending_notification.ringtone_id
                {
                    if self.do_flush_pending_notifications(
                        env,
                        group_key,
                        &mut group,
                        &mut grouped_notifications,
                    ) {
                        force_update = true;
                    }
                    notification_settings_dialog_id = pending_notification.settings_dialog_id;
                    ringtone_id = pending_notification.ringtone_id;
                }
                grouped_notifications.push(pending_notification);
            }
            if self.do_flush_pending_notifications(
                env,
                group_key,
                &mut group,
                &mut grouped_notifications,
            ) {
                force_update = true;
            }
        }

        group.pending_notifications_flush_time = 0.0;
        group.pending_notifications.clear();
        self.on_delayed_notification_update_count_changed(
            env,
            -1,
            group_id.get(),
            "flush_pending_notifications",
        );
        // if we can delete a lot of notifications simultaneously
        if group.notifications.len() > self.keep_notification_group_size + EXTRA_GROUP_SIZE
            && group.r#type.is_database_notification_group_type()
        {
            // keep only keep_notification_group_size_ last notifications in memory
            let erased_count = group.notifications.len() - self.keep_notification_group_size;
            for i in 0..erased_count {
                self.on_notification_removed(env, group.notifications[i].notification_id);
            }
            group.notifications.drain(0..erased_count);
            group.is_loaded_from_database = false;
        }

        self.add_group(final_group_key, group, "flush_pending_notifications");

        if force_update {
            if removed_group_id.is_valid() {
                self.force_flush_pending_updates(
                    env,
                    removed_group_id,
                    "flush_pending_notifications 1",
                );
            }
            self.force_flush_pending_updates(
                env,
                group_key.group_id,
                "flush_pending_notifications 2",
            );
        }
    }

    /// `NotificationManager::flush_all_pending_notifications` (cpp:1633-1649).
    pub fn flush_all_pending_notifications(&mut self, env: &mut impl NotificationManagerEnv) {
        let mut group_ids: Vec<(i32, NotificationGroupId)> = Vec::new();
        for (&group_key, group) in &self.groups {
            if !group.pending_notifications.is_empty() {
                group_ids.push((
                    group.pending_notifications.last().expect("non-empty").date,
                    group_key.group_id,
                ));
            }
        }

        // flush groups in order of the last notification date
        group_ids.sort_unstable();
        for (_, group_id) in group_ids {
            if !env.close_flag() {
                self.flush_pending_notifications_timeout
                    .remove(&(group_id.get() as i64));
            }
            self.flush_pending_notifications(env, group_id);
        }
    }

    /// `load_message_notification_groups_from_database` (cpp:473-497).
    pub fn load_message_notification_groups_from_database(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        limit: i32,
        send_update: bool,
    ) -> i32 {
        assert!(limit > 0);
        if self
            .last_loaded_notification_group_key
            .last_notification_date
            == 0
        {
            // everything was already loaded
            return 0;
        }

        let group_keys = env.get_message_notification_group_keys_from_database(
            self.last_loaded_notification_group_key,
            limit,
        );
        self.last_loaded_notification_group_key = if group_keys.len() == limit as usize {
            *group_keys
                .last()
                .expect("limit is positive and the lengths match")
        } else {
            NotificationGroupKey::default()
        };

        let mut result = 0;
        for &group_key in &group_keys {
            let loaded_key = self
                .get_group_force(env, group_key.group_id, send_update)
                .expect("the loaded group exists");
            assert!(loaded_key.dialog_id.is_valid());
            if self.last_loaded_notification_group_key >= loaded_key {
                result += 1;
            }
        }
        result
    }

    /// `NotificationManager::get_first_notification_id` (cpp:499-508).
    pub fn get_first_notification_id(group: &NotificationGroup) -> NotificationId {
        if !group.notifications.is_empty() {
            return group.notifications[0].notification_id;
        }
        if !group.pending_notifications.is_empty() {
            return group.pending_notifications[0].notification_id;
        }
        NotificationId::default()
    }

    /// `NotificationManager::get_last_notification_id` (cpp:510-518).
    pub fn get_last_notification_id(group: &NotificationGroup) -> NotificationId {
        if !group.pending_notifications.is_empty() {
            return group
                .pending_notifications
                .last()
                .expect("non-empty")
                .notification_id;
        }
        if !group.notifications.is_empty() {
            return group
                .notifications
                .last()
                .expect("non-empty")
                .notification_id;
        }
        NotificationId::default()
    }

    /// `NotificationManager::get_first_object_id` (cpp:520-530).
    pub fn get_first_object_id(group: &NotificationGroup) -> NotificationObjectId {
        // it's fine to return NotificationObjectId() if the first
        // notification has no object_id, because object_id is from the same
        // scope within a notification group
        if !group.notifications.is_empty() {
            return group.notifications[0].r#type.get_object_id();
        }
        if !group.pending_notifications.is_empty() {
            return group.pending_notifications[0].r#type.get_object_id();
        }
        NotificationObjectId::default()
    }

    /// `NotificationManager::get_last_object_id` (cpp:532-542).
    pub fn get_last_object_id(group: &NotificationGroup) -> NotificationObjectId {
        if !group.pending_notifications.is_empty() {
            return group
                .pending_notifications
                .last()
                .expect("non-empty")
                .r#type
                .get_object_id();
        }
        if !group.notifications.is_empty() {
            return group
                .notifications
                .last()
                .expect("non-empty")
                .r#type
                .get_object_id();
        }
        NotificationObjectId::default()
    }

    /// `NotificationManager::get_last_object_id_by_notification_id`
    /// (cpp:544-562).
    pub fn get_last_object_id_by_notification_id(
        group: &NotificationGroup,
        max_notification_id: NotificationId,
    ) -> NotificationObjectId {
        for pending_notification in group.pending_notifications.iter().rev() {
            if pending_notification.notification_id.get() <= max_notification_id.get() {
                let object_id = pending_notification.r#type.get_object_id();
                if object_id.is_valid() {
                    return object_id;
                }
            }
        }
        for notification in group.notifications.iter().rev() {
            if notification.notification_id.get() <= max_notification_id.get() {
                let object_id = notification.r#type.get_object_id();
                if object_id.is_valid() {
                    return object_id;
                }
            }
        }
        NotificationObjectId::default()
    }

    /// `NotificationManager::load_notifications_from_database` (cpp:564-602);
    /// the collapsed form runs the database answer through
    /// [`Self::on_get_notifications_from_database`] synchronously.
    pub fn load_notifications_from_database(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_key: NotificationGroupKey,
        desired_size: usize,
    ) {
        let (should_load, limit, from_notification_id, from_message_id, group_id) = {
            let group = self.groups.get_mut(&group_key).expect("the group exists");
            if !env.use_message_database()
                || group.is_loaded_from_database
                || group.is_being_loaded_from_database
                || group.total_count == 0
                || !group.r#type.is_database_notification_group_type()
            {
                return;
            }

            group.is_being_loaded_from_database = true;

            assert!(desired_size > group.notifications.len());
            let limit = desired_size - group.notifications.len();
            let first_notification_id = Self::get_first_notification_id(group);
            let from_notification_id = if first_notification_id.is_valid() {
                first_notification_id
            } else {
                NotificationId::max()
            };
            let first_object_id = Self::get_first_object_id(group);
            let from_message_id = if first_object_id.is_valid() {
                MessageId::new(first_object_id.get())
            } else {
                MessageId::max()
            };
            (
                true,
                limit,
                from_notification_id,
                from_message_id,
                group_key.group_id,
            )
        };
        let _ = should_load;

        // the C++ UNREACHABLE() calls-group arm cannot exist here: the
        // database-notification-type gate above excludes it
        let r_notifications = env.get_message_notifications_from_database(
            group_key.dialog_id,
            group_id,
            from_notification_id,
            from_message_id,
            limit as i32,
        );
        self.on_get_notifications_from_database(env, group_id, limit, r_notifications);
    }

    /// `NotificationManager::on_get_notifications_from_database`
    /// (cpp:604-645).
    pub fn on_get_notifications_from_database(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_id: NotificationGroupId,
        limit: usize,
        r_notifications: Result<Vec<Notification>, Status>,
    ) {
        let group_key = self.get_group(group_id).expect("the group exists");
        {
            let group = self.groups.get_mut(&group_key).expect("the group exists");
            assert!(group.is_being_loaded_from_database);
            group.is_being_loaded_from_database = false;

            let mut notifications = match r_notifications {
                Ok(notifications) => notifications,
                Err(_) => {
                    group.is_loaded_from_database = true; // do not try again to load it
                    return;
                }
            };

            assert!(limit > 0);
            if notifications.is_empty() {
                group.is_loaded_from_database = true;
            }

            let first_notification_id = Self::get_first_notification_id(group);
            if first_notification_id.is_valid() {
                while !notifications.is_empty()
                    && notifications
                        .last()
                        .expect("non-empty")
                        .notification_id
                        .get()
                        >= first_notification_id.get()
                {
                    // possible if notifications were added after the database request was sent
                    notifications.pop();
                }
            }
            let first_object_id = Self::get_first_object_id(group);
            if first_object_id.is_valid() {
                while !notifications.is_empty()
                    && notifications
                        .last()
                        .expect("non-empty")
                        .r#type
                        .get_object_id()
                        >= first_object_id
                {
                    // possible if notifications were added after the database request was sent
                    notifications.pop();
                }
            }

            self.add_notifications_to_group_begin(env, group_key, notifications);
        }

        let group_key = self.get_group(group_id).expect("the group still exists");
        if self.max_notification_group_size
            > self
                .groups
                .get(&group_key)
                .expect("the group exists")
                .notifications
                .len()
        {
            self.load_notifications_from_database(
                env,
                group_key,
                self.keep_notification_group_size,
            );
        }
    }

    /// `NotificationManager::add_notifications_to_group_begin`
    /// (cpp:647-750).
    pub fn add_notifications_to_group_begin(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_key: NotificationGroupKey,
        mut notifications: Vec<Notification>,
    ) {
        let dialog_id = group_key.dialog_id;
        notifications.retain(|notification| {
            notification
                .r#type
                .get_notification_type_object(env, dialog_id)
                .is_some()
        });

        if notifications.is_empty() {
            return;
        }

        let mut final_group_key = group_key;
        for notification in &notifications {
            if notification.date > final_group_key.last_notification_date {
                final_group_key.last_notification_date = notification.date;
            }
        }
        assert!(
            final_group_key.last_notification_date != 0,
            "{final_group_key}"
        );

        let is_position_changed =
            final_group_key.last_notification_date != group_key.last_notification_date;

        let mut group = self.groups.remove(&group_key).expect("the group exists");
        if is_position_changed {
            let erased_count = self.group_keys.remove(&group_key.group_id).is_some();
            assert!(erased_count);
        }

        let last_group_key = self.get_last_updated_group_key();
        let (was_updated, is_updated) = if is_position_changed {
            (
                group_key.last_notification_date != 0 && group_key < last_group_key,
                final_group_key.last_notification_date != 0 && final_group_key < last_group_key,
            )
        } else {
            assert!(group_key.last_notification_date != 0);
            let updated = last_group_key >= group_key;
            (updated, updated)
        };

        if !is_updated {
            assert!(!was_updated);
            let mut merged = notifications;
            merged.append(&mut group.notifications);
            group.notifications = merged;
        } else {
            if !was_updated {
                if last_group_key.last_notification_date != 0 {
                    // need to remove the last notification group to not
                    // exceed max_notification_group_count_
                    let last_group = self
                        .groups
                        .get(&last_group_key)
                        .cloned()
                        .expect("the group exists");
                    self.send_remove_group_update(env, last_group_key, &last_group, Vec::new());
                }
                self.send_add_group_update(
                    env,
                    group_key,
                    &group,
                    "add_notifications_to_group_begin",
                );
            }

            let mut added_notifications: Vec<td_api::Notification> = Vec::new();
            for notification in &notifications {
                let notification_object =
                    get_notification_object(env, group_key.dialog_id, notification);
                assert!(notification_object.r#type.is_some());
                added_notifications.push(notification_object);
            }

            let old_notification_count = group.notifications.len();
            let updated_notification_count = self
                .max_notification_group_size
                .saturating_sub(old_notification_count);
            if added_notifications.len() > updated_notification_count {
                let excess = added_notifications.len() - updated_notification_count;
                added_notifications.drain(0..excess);
            }
            let mut new_notification_count = self
                .keep_notification_group_size
                .saturating_sub(old_notification_count);
            if new_notification_count > notifications.len() {
                new_notification_count = notifications.len();
            }
            if new_notification_count != 0 {
                let drain_start = notifications.len() - new_notification_count;
                let mut merged: Vec<Notification> = notifications.drain(drain_start..).collect();
                merged.append(&mut group.notifications);
                group.notifications = merged;
            }

            if !added_notifications.is_empty() {
                self.add_update_notification_group(
                    env,
                    td_api::updateNotificationGroup {
                        notification_group_id: group_key.group_id.get(),
                        r#type: Some(group.r#type.get_notification_group_type_object()),
                        chat_id: env.get_chat_id_object(group_key.dialog_id),
                        notification_settings_chat_id: 0,
                        notification_sound_id: 0,
                        total_count: group.total_count,
                        added_notifications,
                        removed_notification_ids: Vec::new(),
                    },
                );
            }
        }

        if is_position_changed {
            self.add_group(final_group_key, group, "add_notifications_to_group_begin");
        } else {
            assert!(group_key.last_notification_date == 0 || !group.notifications.is_empty());
            self.groups.insert(group_key, group);
        }
    }

    /// `NotificationManager::get_notification_delay_ms` (cpp:841-878).
    pub fn get_notification_delay_ms(
        &self,
        env: &impl NotificationManagerEnv,
        dialog_id: DialogId,
        notification: &PendingNotification,
        min_delay_ms: i32,
    ) -> i32 {
        if dialog_id.get_type() == DialogType::SecretChat {
            return MIN_NOTIFICATION_DELAY_MS; // there is no reason to delay notifications in secret chats
        }
        if !notification.r#type.can_be_delayed() {
            return MIN_NOTIFICATION_DELAY_MS;
        }

        let server_time = env.server_time();
        let delay_ms = {
            let online_info = env.get_my_online_status();
            if !online_info.is_online_local && online_info.is_online_remote {
                // If we are offline, but online from some other client, then
                // delay notification for 'notification_cloud_delay' seconds.
                self.notification_cloud_delay_ms
            } else if !online_info.is_online_local
                && online_info.was_online_remote
                    > online_info
                        .was_online_local
                        .max(server_time - self.online_cloud_timeout_ms as f64 * 1e-3)
            {
                // If we are offline, but was online from some other client in
                // the last 'online_cloud_timeout' seconds after we had gone
                // offline, then delay notification for 'notification_cloud_delay'
                // seconds.
                self.notification_cloud_delay_ms
            } else if online_info.is_online_remote {
                // If some other client is online, then delay notification for
                // 'notification_default_delay' seconds.
                self.notification_default_delay_ms
            } else {
                // otherwise send the update without additional delay
                0
            }
        };

        let passed_time_ms = ((server_time - f64::from(notification.date) - 1.0)
            .clamp(0.0, 1_000_000.0)
            * 1000.0) as i32;
        (min_delay_ms.max(delay_ms) - passed_time_ms).max(MIN_NOTIFICATION_DELAY_MS)
    }

    /// `NotificationManager::add_notification` (cpp:880-953).
    #[allow(clippy::too_many_arguments)]
    pub fn add_notification(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_id: NotificationGroupId,
        group_type: NotificationGroupType,
        dialog_id: DialogId,
        date: i32,
        notification_settings_dialog_id: DialogId,
        disable_notification: bool,
        ringtone_id: i64,
        min_delay_ms: i32,
        notification_id: NotificationId,
        type_: NotificationType,
        source: &str,
    ) {
        if env.is_disabled() || self.max_notification_group_count == 0 {
            self.on_notification_removed(env, notification_id);
            return;
        }

        assert!(group_id.is_valid());
        assert!(dialog_id.is_valid());
        assert!(notification_settings_dialog_id.is_valid());
        assert!(notification_id.is_valid(), "{notification_id} {source}");
        if !type_.is_temporary() {
            self.remove_temporary_notifications(env, group_id, "add_notification");
        }

        let group_key = match self.get_group_force(env, group_id, true) {
            Some(group_key) => group_key,
            None => {
                let group_key = NotificationGroupKey::new(group_id, dialog_id, 0);
                self.add_group(group_key, NotificationGroup::default(), "add_notification");
                group_key
            }
        };
        {
            let group = self.groups.get_mut(&group_key).expect("the group exists");
            if group.notifications.is_empty() && group.pending_notifications.is_empty() {
                group.r#type = group_type;
            }
            assert!(group.r#type == group_type);
        }

        let (last_notification_id, last_object_id) = {
            let group = self.groups.get(&group_key).expect("the group exists");
            (
                Self::get_last_notification_id(group),
                Self::get_last_object_id(group),
            )
        };
        if notification_id.get() <= last_notification_id.get() {
            tdutils::log_error!(
                "Failed to add {notification_id} to {group_id}, because have already added {last_notification_id}"
            );
            self.on_notification_removed(env, notification_id);
            return;
        }
        let object_id = type_.get_object_id();
        if object_id.is_valid() && object_id <= last_object_id {
            tdutils::log_error!(
                "Failed to add {notification_id} to {group_id}, because have already added a notification about {last_object_id}"
            );
            self.on_notification_removed(env, notification_id);
            return;
        }
        if notification_settings_dialog_id != dialog_id {
            env.force_create_dialog(notification_settings_dialog_id, "add_notification", true);
        }

        let notification = PendingNotification {
            date,
            settings_dialog_id: notification_settings_dialog_id,
            disable_notification,
            ringtone_id: if disable_notification { 0 } else { ringtone_id },
            notification_id,
            r#type: type_,
        };

        let delay_ms = self.get_notification_delay_ms(env, dialog_id, &notification, min_delay_ms);
        let flush_time = delay_ms as f64 * 0.001 + env.now();

        let was_empty = {
            let group = self.groups.get_mut(&group_key).expect("the group exists");
            if group.pending_notifications_flush_time == 0.0
                || flush_time < group.pending_notifications_flush_time
            {
                group.pending_notifications_flush_time = flush_time;
                if !env.close_flag() {
                    self.flush_pending_notifications_timeout
                        .insert(group_id.get() as i64, flush_time);
                }
            }
            group.pending_notifications.is_empty()
        };
        if was_empty {
            self.on_delayed_notification_update_count_changed(env, 1, group_id.get(), source);
        }
        self.groups
            .get_mut(&group_key)
            .expect("the group exists")
            .pending_notifications
            .push(notification);
    }

    /// `NotificationManager::remove_temporary_notifications` (cpp:2158-2258).
    pub fn remove_temporary_notifications(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_id: NotificationGroupId,
        _source: &str,
    ) {
        assert!(group_id.is_valid());

        if env.is_disabled() || self.max_notification_group_count == 0 {
            return;
        }

        let Some(group_key) = self.get_group(group_id) else {
            return;
        };

        if Self::get_temporary_notification_total_count(
            self.groups.get(&group_key).expect("the group exists"),
        ) == 0
        {
            return;
        }

        // remove trailing temporary pending notifications
        loop {
            let notification_id = {
                let group = self.groups.get(&group_key).expect("the group exists");
                match group.pending_notifications.last() {
                    Some(last) if last.r#type.is_temporary() => last.notification_id,
                    _ => break,
                }
            };
            // the notification is still pending, just delete it
            self.on_notification_removed(env, notification_id);
            let is_empty = {
                let group = self.groups.get_mut(&group_key).expect("the group exists");
                group.pending_notifications.pop();
                group.pending_notifications.is_empty()
            };
            if is_empty {
                self.groups
                    .get_mut(&group_key)
                    .expect("the group exists")
                    .pending_notifications_flush_time = 0.0;
                if !env.close_flag() {
                    self.flush_pending_notifications_timeout
                        .remove(&(group_id.get() as i64));
                }
                self.on_delayed_notification_update_count_changed(
                    env,
                    -1,
                    group_id.get(),
                    "remove_temporary_notifications",
                );
            }
        }

        let old_group_size = self
            .groups
            .get(&group_key)
            .expect("the group exists")
            .notifications
            .len();
        let mut notification_pos = old_group_size;
        {
            let group = self.groups.get(&group_key).expect("the group exists");
            for (pos, notification) in group.notifications.iter().enumerate() {
                if notification.r#type.is_temporary() {
                    notification_pos = pos;
                    break;
                }
            }
        }
        let removed_notification_count = (old_group_size - notification_pos) as i32;
        if removed_notification_count == 0 {
            assert!(
                Self::get_temporary_notification_total_count(
                    self.groups.get(&group_key).expect("the group exists")
                ) == 0
            );
            return;
        }

        {
            let group = self.groups.get_mut(&group_key).expect("the group exists");
            if group.total_count < removed_notification_count {
                tdutils::log_error!(
                    "Total notification count became negative in {group_id} after removing {removed_notification_count} temporary notifications"
                );
                group.total_count = 0;
            } else {
                group.total_count -= removed_notification_count;
            }
        }
        let mut removed_notification_ids: Vec<i32> = Vec::new();
        for i in notification_pos..old_group_size {
            let notification_id = {
                let group = self.groups.get(&group_key).expect("the group exists");
                assert!(
                    group.notifications[i].r#type.is_temporary(),
                    "{}",
                    group.notifications[i]
                );
                group.notifications[i].notification_id
            };
            self.on_notification_removed(env, notification_id);
            if i + self.max_notification_group_size >= old_group_size {
                removed_notification_ids.push(notification_id.get());
            }
        }
        self.groups
            .get_mut(&group_key)
            .expect("the group exists")
            .notifications
            .truncate(notification_pos);
        assert!(!removed_notification_ids.is_empty());

        let mut added_notifications: Vec<td_api::Notification> = Vec::new();
        if old_group_size >= self.max_notification_group_size {
            let mut added_notification_count = 0;
            {
                let group = self.groups.get(&group_key).expect("the group exists");
                let begin =
                    (old_group_size - self.max_notification_group_size).min(notification_pos);
                for i in (0..begin).rev() {
                    if added_notification_count >= removed_notification_ids.len() {
                        break;
                    }
                    added_notification_count += 1;
                    let notification_object =
                        get_notification_object(env, group_key.dialog_id, &group.notifications[i]);
                    if notification_object.r#type.is_some() {
                        added_notifications.push(notification_object);
                    }
                }
            }
            if added_notification_count < removed_notification_ids.len()
                && self.max_notification_group_size
                    > self
                        .groups
                        .get(&group_key)
                        .expect("the group exists")
                        .notifications
                        .len()
            {
                self.load_notifications_from_database(
                    env,
                    group_key,
                    self.keep_notification_group_size,
                );
            }
            added_notifications.reverse();
        }
        assert!(
            Self::get_temporary_notification_total_count(
                self.groups.get(&group_key).expect("the group exists")
            ) == 0
        );

        let group = self
            .groups
            .get(&group_key)
            .expect("the group exists")
            .clone();
        self.on_notifications_removed(
            env,
            group_key,
            group,
            added_notifications,
            removed_notification_ids,
            false,
        );

        self.remove_added_notifications_from_pending_updates(group_id, |notification| {
            matches!(
                notification.r#type,
                Some(td_api::NotificationType::notificationTypeNewPushMessage(_))
            )
        });
    }

    /// `NotificationManager::get_temporary_notification_total_count`
    /// (cpp:2260-2275).
    pub fn get_temporary_notification_total_count(group: &NotificationGroup) -> i32 {
        let mut result = 0;
        for notification in group.notifications.iter().rev() {
            if !notification.r#type.is_temporary() {
                break;
            }
            result += 1;
        }
        for pending_notification in group.pending_notifications.iter().rev() {
            if !pending_notification.r#type.is_temporary() {
                break;
            }
            result += 1;
        }
        result
    }

    /// `NotificationManager::on_notifications_removed` (cpp:1744-1822).
    #[allow(clippy::too_many_arguments)]
    pub fn on_notifications_removed(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_key: NotificationGroupKey,
        group: NotificationGroup,
        added_notifications: Vec<td_api::Notification>,
        removed_notification_ids: Vec<i32>,
        force_update: bool,
    ) {
        let mut final_group_key = group_key;
        final_group_key.last_notification_date = 0;
        for notification in &group.notifications {
            if notification.date > final_group_key.last_notification_date {
                final_group_key.last_notification_date = notification.date;
            }
        }

        let is_position_changed =
            final_group_key.last_notification_date != group_key.last_notification_date;

        self.groups.remove(&group_key);
        if is_position_changed {
            let erased_count = self.group_keys.remove(&group_key.group_id).is_some();
            assert!(erased_count);
        }

        let mut last_group_key = self.get_last_updated_group_key();
        let (was_updated, is_updated) = if is_position_changed {
            (
                group_key.last_notification_date != 0 && group_key < last_group_key,
                final_group_key.last_notification_date != 0 && final_group_key < last_group_key,
            )
        } else {
            let updated = group_key.last_notification_date != 0 && (last_group_key >= group_key);
            (updated, updated)
        };

        if !was_updated {
            assert!(!is_updated);
            if final_group_key.last_notification_date == 0 && group.total_count == 0 {
                // send an update about an empty invisible group anyway
                self.add_update_notification_group(
                    env,
                    td_api::updateNotificationGroup {
                        notification_group_id: group_key.group_id.get(),
                        r#type: Some(group.r#type.get_notification_group_type_object()),
                        chat_id: env.get_chat_id_object(group_key.dialog_id),
                        notification_settings_chat_id: 0,
                        notification_sound_id: 0,
                        total_count: 0,
                        added_notifications: Vec::new(),
                        removed_notification_ids: Vec::new(),
                    },
                );
            }
        } else if is_updated {
            // the group is still visible
            self.add_update_notification_group(
                env,
                td_api::updateNotificationGroup {
                    notification_group_id: group_key.group_id.get(),
                    r#type: Some(group.r#type.get_notification_group_type_object()),
                    chat_id: env.get_chat_id_object(group_key.dialog_id),
                    notification_settings_chat_id: 0,
                    notification_sound_id: 0,
                    total_count: group.total_count,
                    added_notifications,
                    removed_notification_ids,
                },
            );
        } else {
            // the group needs to be removed
            self.send_remove_group_update(env, group_key, &group, removed_notification_ids);
            if last_group_key.last_notification_date != 0 {
                // need to add the new last notification group
                let last_group = self
                    .groups
                    .get(&last_group_key)
                    .cloned()
                    .expect("the group exists");
                self.send_add_group_update(
                    env,
                    last_group_key,
                    &last_group,
                    "on_notifications_removed",
                );
            }
        }

        if is_position_changed {
            self.add_group(final_group_key, group, "on_notifications_removed");

            last_group_key = self.get_last_updated_group_key();
        } else {
            assert!(group_key.last_notification_date == 0 || !group.notifications.is_empty());
            self.groups.insert(group_key, group);
        }

        if force_update {
            self.force_flush_pending_updates(env, group_key.group_id, "on_notifications_removed");
        }

        if self.last_loaded_notification_group_key < last_group_key {
            let limit = (self.max_notification_group_count.max(10) as i32) / 2;
            self.load_message_notification_groups_from_database(env, limit, true);
        }
    }

    /// `NotificationManager::remove_added_notifications_from_pending_updates`
    /// (cpp:1824-1864).
    pub fn remove_added_notifications_from_pending_updates(
        &mut self,
        group_id: NotificationGroupId,
        is_removed: impl Fn(&td_api::Notification) -> bool,
    ) {
        let Some(updates) = self.pending_updates.get_mut(&group_id.get()) else {
            return;
        };

        let mut removed_notification_ids: HashSet<i32> = HashSet::new();
        for slot in updates.iter_mut() {
            let Some(update) = slot.as_mut() else {
                continue;
            };
            let mut is_whole_update_removed = false;
            match update {
                td_api::Update::updateNotificationGroup(group_update) => {
                    if !removed_notification_ids.is_empty()
                        && !group_update.removed_notification_ids.is_empty()
                    {
                        group_update
                            .removed_notification_ids
                            .retain(|notification_id| {
                                !removed_notification_ids.contains(notification_id)
                            });
                    }
                    let mut newly_removed: Vec<i32> = Vec::new();
                    group_update.added_notifications.retain(|notification| {
                        if is_removed(notification) {
                            assert!(notification.id != 0);
                            newly_removed.push(notification.id);
                            return false;
                        }
                        true
                    });
                    removed_notification_ids.extend(newly_removed);
                }
                td_api::Update::updateNotification(notification_update) => {
                    let notification = notification_update
                        .notification
                        .as_ref()
                        .expect("pending updates hold a notification");
                    if is_removed(notification) {
                        assert!(notification.id != 0);
                        // 4c6377aa7 — an edit doesn't make a notification
                        // new; its later removal stays live unless an
                        // addition was cancelled too (the id is NOT
                        // remembered as already removed).
                        is_whole_update_removed = true;
                    }
                }
                _ => unreachable!("a pending notification update has a known constructor"),
            }
            if is_whole_update_removed {
                *slot = None;
            }
        }
    }

    // ---- M.13.6: edit/remove/count family (cpp:1651-2471) ----

    /// `NotificationManager::edit_notification` (cpp:1651-1701).
    pub fn edit_notification(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_id: NotificationGroupId,
        notification_id: NotificationId,
        type_: NotificationType,
    ) {
        if env.is_disabled() || self.max_notification_group_count == 0 {
            return;
        }
        if !group_id.is_valid() {
            return;
        }

        assert!(notification_id.is_valid());

        let Some(group_key) = self.get_group(group_id) else {
            return;
        };

        let mut edited_notification_pos = None;
        {
            let group = self.groups.get(&group_key).expect("the group exists");
            for (i, notification) in group.notifications.iter().enumerate() {
                if notification.notification_id != notification_id {
                    continue;
                }
                if notification.r#type.get_object_id() != type_.get_object_id()
                    || notification.r#type.is_temporary() != type_.is_temporary()
                {
                    tdutils::log_error!(
                        "Ignore edit of {} with {}, because previous type is {}",
                        notification_id,
                        type_,
                        notification.r#type
                    );
                    return;
                }
                edited_notification_pos = Some(i);
                break;
            }
        }
        if let Some(i) = edited_notification_pos {
            {
                let group = self.groups.get_mut(&group_key).expect("the group exists");
                group.notifications[i].r#type = type_;
            }
            let group_size = self
                .groups
                .get(&group_key)
                .expect("the group exists")
                .notifications
                .len();
            if i + self.max_notification_group_size >= group_size
                && (self.get_last_updated_group_key() >= group_key)
            {
                assert!(group_key.last_notification_date != 0);
                let notification = self
                    .groups
                    .get(&group_key)
                    .expect("the group exists")
                    .notifications[i]
                    .clone();
                self.add_update_notification(
                    env,
                    group_key.group_id,
                    group_key.dialog_id,
                    &notification,
                );
            }
            return;
        }

        let group = self.groups.get_mut(&group_key).expect("the group exists");
        for pending_notification in group.pending_notifications.iter_mut() {
            if pending_notification.notification_id != notification_id {
                continue;
            }
            if pending_notification.r#type.get_object_id() != type_.get_object_id()
                || pending_notification.r#type.is_temporary() != type_.is_temporary()
            {
                tdutils::log_error!(
                    "Ignore edit of {} with {}, because previous type is {}",
                    notification_id,
                    type_,
                    pending_notification.r#type
                );
                return;
            }

            pending_notification.r#type = type_;
            return;
        }
    }

    /// `NotificationManager::remove_notification` (cpp:1866-1977).
    #[allow(clippy::too_many_arguments)]
    pub fn remove_notification(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_id: NotificationGroupId,
        notification_id: NotificationId,
        is_permanent: bool,
        force_update: bool,
        mut promise: Promise<()>,
        source: &str,
    ) {
        if !group_id.is_valid() {
            promise.set_error(Status::error(
                400,
                "Notification group identifier is invalid",
            ));
            return;
        }
        if !notification_id.is_valid() {
            promise.set_error(Status::error(400, "Notification identifier is invalid"));
            return;
        }

        if env.is_disabled() || self.max_notification_group_count == 0 {
            promise.set_value(());
            return;
        }

        let Some(group_key) = self.get_group_force(env, group_id, true) else {
            promise.set_value(());
            return;
        };

        if !is_permanent {
            let group_type = self
                .groups
                .get(&group_key)
                .expect("the group exists")
                .r#type;
            match group_type {
                NotificationGroupType::Messages
                | NotificationGroupType::Mentions
                | NotificationGroupType::SecretChat => {
                    env.remove_message_notification(group_key.dialog_id, group_id, notification_id);
                }
                NotificationGroupType::Calls => {
                    // nothing to do
                }
            }
        }

        let pending_notification_pos = {
            let group = self.groups.get(&group_key).expect("the group exists");
            group
                .pending_notifications
                .iter()
                .position(|pending_notification| {
                    pending_notification.notification_id == notification_id
                })
        };
        if let Some(pending_notification_pos) = pending_notification_pos {
            // notification is still pending, just delete it
            self.on_notification_removed(env, notification_id);
            {
                let group = self.groups.get_mut(&group_key).expect("the group exists");
                group.pending_notifications.remove(pending_notification_pos);
                if group.pending_notifications.is_empty() {
                    group.pending_notifications_flush_time = 0.0;
                    if !env.close_flag() {
                        self.flush_pending_notifications_timeout
                            .remove(&(group_id.get() as i64));
                    }
                }
            }
            if self
                .groups
                .get(&group_key)
                .expect("the group exists")
                .pending_notifications
                .is_empty()
            {
                self.on_delayed_notification_update_count_changed(
                    env,
                    -1,
                    group_id.get(),
                    "remove_notification",
                );
            }
            promise.set_value(());
            return;
        }

        let old_group_size = self
            .groups
            .get(&group_key)
            .expect("the group exists")
            .notifications
            .len();
        let mut notification_pos = old_group_size;
        {
            let group = self.groups.get(&group_key).expect("the group exists");
            for pos in 0..group.notifications.len() {
                if group.notifications[pos].notification_id == notification_id {
                    notification_pos = pos;
                    break;
                }
            }
        }
        let is_found = notification_pos != old_group_size;
        if is_found {
            self.on_notification_removed(env, notification_id);
        }

        let have_all_notifications = !self
            .groups
            .get(&group_key)
            .expect("the group exists")
            .r#type
            .is_partial_notification_group_type();
        let mut is_total_count_changed = false;
        if (!have_all_notifications && is_permanent) || (have_all_notifications && is_found) {
            let group = self.groups.get_mut(&group_key).expect("the group exists");
            if group.total_count == 0 {
                tdutils::log_error!(
                    "Total notification count became negative in group {} after removing {} with is_permanent = {}, is_found = {}, force_update = {} from {}",
                    group_id,
                    notification_id,
                    is_permanent,
                    is_found,
                    force_update,
                    source
                );
            } else {
                group.total_count -= 1;
                is_total_count_changed = true;
            }
        }
        if is_found {
            let group = self.groups.get_mut(&group_key).expect("the group exists");
            group.notifications.remove(notification_pos);
        }

        let mut added_notifications: Vec<td_api::Notification> = Vec::new();
        let mut removed_notification_ids: Vec<i32> = Vec::new();
        assert!(self.max_notification_group_size > 0);
        if is_found && notification_pos + self.max_notification_group_size >= old_group_size {
            removed_notification_ids.push(notification_id.get());
            if old_group_size > self.max_notification_group_size {
                let group = self.groups.get(&group_key).expect("the group exists");
                let notification_object = get_notification_object(
                    env,
                    group_key.dialog_id,
                    &group.notifications[old_group_size - self.max_notification_group_size - 1],
                );
                if notification_object.r#type.is_some() {
                    added_notifications.push(notification_object);
                }
            }
            if added_notifications.is_empty()
                && self.max_notification_group_size
                    > self
                        .groups
                        .get(&group_key)
                        .expect("the group exists")
                        .notifications
                        .len()
            {
                self.load_notifications_from_database(
                    env,
                    group_key,
                    self.keep_notification_group_size,
                );
            }
        }

        if is_total_count_changed || !removed_notification_ids.is_empty() {
            let group = self
                .groups
                .get(&group_key)
                .expect("the group exists")
                .clone();
            self.on_notifications_removed(
                env,
                group_key,
                group,
                added_notifications,
                removed_notification_ids,
                force_update,
            );
        }

        self.remove_added_notifications_from_pending_updates(group_id, |notification| {
            notification.id == notification_id.get()
        });

        promise.set_value(());
    }

    /// `NotificationManager::remove_temporary_notification_by_object_id`
    /// (cpp:1979-2008).
    pub fn remove_temporary_notification_by_object_id(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_id: NotificationGroupId,
        object_id: NotificationObjectId,
        force_update: bool,
        _source: &str,
    ) {
        if !group_id.is_valid() {
            return;
        }

        assert!(object_id.is_valid());

        let Some(group_key) = self.get_group(group_id) else {
            return;
        };

        let found = {
            let group = self.groups.get(&group_key).expect("the group exists");
            group
                .pending_notifications
                .iter()
                .find(|pending_notification| {
                    pending_notification.r#type.get_object_id() == object_id
                })
                .map(|pending_notification| {
                    (
                        pending_notification.notification_id,
                        pending_notification.r#type.get_file_ids(),
                    )
                })
        };
        if let Some((notification_id, file_ids)) = found {
            for file_id in file_ids {
                env.delete_file(file_id);
            }
            self.remove_notification(
                env,
                group_id,
                notification_id,
                true,
                force_update,
                Promise::default(),
                "remove_temporary_notification_by_object_id",
            );
        }

        // the group is looked up again — the pending removal may have
        // changed it
        let Some(group_key) = self.get_group(group_id) else {
            return;
        };
        let found = {
            let group = self.groups.get(&group_key).expect("the group exists");
            group
                .notifications
                .iter()
                .find(|notification| notification.r#type.get_object_id() == object_id)
                .map(|notification| {
                    (
                        notification.notification_id,
                        notification.r#type.get_file_ids(),
                    )
                })
        };
        if let Some((notification_id, file_ids)) = found {
            for file_id in file_ids {
                env.delete_file(file_id);
            }
            self.remove_notification(
                env,
                group_id,
                notification_id,
                true,
                force_update,
                Promise::default(),
                "remove_temporary_notification_by_object_id",
            );
        }
    }

    /// `NotificationManager::remove_notification_group` (cpp:2010-2156).
    #[allow(clippy::too_many_arguments)]
    pub fn remove_notification_group(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_id: NotificationGroupId,
        mut max_notification_id: NotificationId,
        max_object_id: NotificationObjectId,
        mut new_total_count: i32,
        force_update: bool,
        mut promise: Promise<()>,
    ) {
        if !group_id.is_valid() {
            promise.set_error(Status::error(400, "Group identifier is invalid"));
            return;
        }
        if !max_notification_id.is_valid() && !max_object_id.is_valid() {
            promise.set_error(Status::error(400, "Notification identifier is invalid"));
            return;
        }

        if env.is_disabled() || self.max_notification_group_count == 0 {
            promise.set_value(());
            return;
        }

        if new_total_count == 0 {
            self.remove_temporary_notifications(env, group_id, "remove_notification_group");
        }

        let Some(group_key) = self.get_group_force(env, group_id, true) else {
            promise.set_value(());
            return;
        };

        if max_notification_id.is_valid() {
            if max_notification_id.get() > self.current_notification_id.get() {
                max_notification_id = self.current_notification_id;
            }
            let group_type = self
                .groups
                .get(&group_key)
                .expect("the group exists")
                .r#type;
            match group_type {
                NotificationGroupType::Messages
                | NotificationGroupType::Mentions
                | NotificationGroupType::SecretChat => {
                    let last_object_id = Self::get_last_object_id_by_notification_id(
                        self.groups.get(&group_key).expect("the group exists"),
                        max_notification_id,
                    );
                    env.remove_message_notifications(
                        group_key.dialog_id,
                        group_id,
                        max_notification_id,
                        MessageId::new(last_object_id.get()),
                    );
                }
                NotificationGroupType::Calls => {
                    // nothing to do
                }
            }
        }

        // delete the prefix of pending notifications up to the last match
        let pending_delete_len = {
            let group = self.groups.get(&group_key).expect("the group exists");
            let mut pending_delete_len = 0;
            for (i, pending_notification) in group.pending_notifications.iter().enumerate() {
                if pending_notification.notification_id.get() <= max_notification_id.get()
                    || (max_object_id.is_valid()
                        && pending_notification.r#type.get_object_id() <= max_object_id)
                {
                    pending_delete_len = i + 1;
                }
            }
            pending_delete_len
        };
        if pending_delete_len > 0 {
            let deleted_ids: Vec<NotificationId> = {
                let group = self.groups.get(&group_key).expect("the group exists");
                group.pending_notifications[..pending_delete_len]
                    .iter()
                    .map(|pending_notification| pending_notification.notification_id)
                    .collect()
            };
            for notification_id in deleted_ids {
                self.on_notification_removed(env, notification_id);
            }
            {
                let group = self.groups.get_mut(&group_key).expect("the group exists");
                group.pending_notifications.drain(0..pending_delete_len);
                if group.pending_notifications.is_empty() {
                    group.pending_notifications_flush_time = 0.0;
                    if !env.close_flag() {
                        self.flush_pending_notifications_timeout
                            .remove(&(group_id.get() as i64));
                    }
                }
            }
            if self
                .groups
                .get(&group_key)
                .expect("the group exists")
                .pending_notifications
                .is_empty()
            {
                self.on_delayed_notification_update_count_changed(
                    env,
                    -1,
                    group_id.get(),
                    "remove_notification_group",
                );
            }
        }
        if new_total_count != -1 {
            let group = self.groups.get(&group_key).expect("the group exists");
            new_total_count += Self::get_temporary_notification_total_count(group);
            new_total_count -= group.pending_notifications.len() as i32;
            if new_total_count < 0 {
                tdutils::log_error!(
                    "Have wrong new_total_count {} + {}",
                    new_total_count,
                    group.pending_notifications.len()
                );
            }
        }

        let old_group_size = self
            .groups
            .get(&group_key)
            .expect("the group exists")
            .notifications
            .len();
        let mut notification_delete_end = old_group_size;
        let mut removed_by_object_ids: Vec<NotificationId> = Vec::new();
        {
            let group = self.groups.get(&group_key).expect("the group exists");
            for notification in &group.notifications {
                if notification.notification_id.get() > max_notification_id.get()
                    && (!max_object_id.is_valid()
                        || notification.r#type.get_object_id() > max_object_id)
                {
                    notification_delete_end = removed_by_object_ids.len();
                    break;
                }
                removed_by_object_ids.push(notification.notification_id);
            }
        }
        for notification_id in removed_by_object_ids {
            self.on_notification_removed(env, notification_id);
        }

        let is_found = notification_delete_end != 0;

        let mut removed_notification_ids: Vec<i32> = Vec::new();
        if is_found && notification_delete_end + self.max_notification_group_size > old_group_size {
            let start = old_group_size.saturating_sub(self.max_notification_group_size);
            let group = self.groups.get(&group_key).expect("the group exists");
            for notification in &group.notifications[start..notification_delete_end] {
                removed_notification_ids.push(notification.notification_id.get());
            }
        }

        if is_found {
            let group = self.groups.get_mut(&group_key).expect("the group exists");
            group.notifications.drain(0..notification_delete_end);
        }
        {
            let group = self.groups.get_mut(&group_key).expect("the group exists");
            if !group.r#type.is_partial_notification_group_type() {
                new_total_count = group.notifications.len() as i32;
            }
            if group.total_count == new_total_count {
                new_total_count = -1;
            }
            if new_total_count != -1 {
                group.total_count = new_total_count;
            }
        }

        if new_total_count != -1 || !removed_notification_ids.is_empty() {
            let group = self
                .groups
                .get(&group_key)
                .expect("the group exists")
                .clone();
            self.on_notifications_removed(
                env,
                group_key,
                group,
                Vec::new(),
                removed_notification_ids,
                force_update,
            );
        } else if force_update {
            self.force_flush_pending_updates(env, group_id, "remove_notification_group");
        }

        if max_notification_id.is_valid() {
            let max_notification_id = max_notification_id.get();
            self.remove_added_notifications_from_pending_updates(group_id, move |notification| {
                notification.id <= max_notification_id
            });
        } else {
            let max_object_id = max_object_id.get();
            self.remove_added_notifications_from_pending_updates(group_id, move |notification| {
                match notification.r#type.as_ref() {
                    Some(td_api::NotificationType::notificationTypeNewMessage(new_message)) => {
                        new_message
                            .message
                            .as_ref()
                            .is_some_and(|message| message.id <= max_object_id)
                    }
                    Some(td_api::NotificationType::notificationTypeNewPushMessage(
                        push_message,
                    )) => push_message.message_id <= max_object_id,
                    _ => false,
                }
            });
        }

        promise.set_value(());
    }

    /// `NotificationManager::set_notification_total_count` (cpp:2277-2314).
    pub fn set_notification_total_count(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_id: NotificationGroupId,
        new_total_count: i32,
    ) {
        if !group_id.is_valid() {
            return;
        }
        if env.is_disabled() || self.max_notification_group_count == 0 {
            return;
        }

        let Some(group_key) = self.get_group_force(env, group_id, true) else {
            return;
        };

        let mut new_total_count = new_total_count;
        {
            let group = self.groups.get(&group_key).expect("the group exists");
            new_total_count += Self::get_temporary_notification_total_count(group);
            new_total_count -= group.pending_notifications.len() as i32;
            if new_total_count < 0 {
                tdutils::log_error!(
                    "Have wrong new_total_count {} after removing {} pending notifications",
                    new_total_count,
                    group.pending_notifications.len()
                );
                return;
            }
            if new_total_count < group.notifications.len() as i32 {
                tdutils::log_error!(
                    "Have wrong new_total_count {} less than number of known notifications {}",
                    new_total_count,
                    group.notifications.len()
                );
                return;
            }

            assert!(group.r#type != NotificationGroupType::Calls);
            if group.total_count == new_total_count {
                return;
            }
        }

        let group = self.groups.get_mut(&group_key).expect("the group exists");
        group.total_count = new_total_count;

        let group = self
            .groups
            .get(&group_key)
            .expect("the group exists")
            .clone();
        self.on_notifications_removed(env, group_key, group, Vec::new(), Vec::new(), false);
    }

    /// `NotificationManager::get_notification_group_message_ids`
    /// (cpp:2316-2347).
    pub fn get_notification_group_message_ids(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_id: NotificationGroupId,
    ) -> Vec<MessageId> {
        assert!(group_id.is_valid());
        if env.is_disabled() || self.max_notification_group_count == 0 {
            return Vec::new();
        }

        let Some(group_key) = self.get_group_force(env, group_id, true) else {
            return Vec::new();
        };

        let group = self.groups.get(&group_key).expect("the group exists");
        if group.r#type != NotificationGroupType::Mentions
            && group.r#type != NotificationGroupType::Messages
        {
            return Vec::new();
        }

        let mut message_ids: Vec<MessageId> = Vec::new();
        for notification in &group.notifications {
            let object_id = notification.r#type.get_object_id();
            if object_id.is_valid() {
                message_ids.push(MessageId::new(object_id.get()));
            }
        }
        for pending_notification in &group.pending_notifications {
            let object_id = pending_notification.r#type.get_object_id();
            if object_id.is_valid() {
                message_ids.push(MessageId::new(object_id.get()));
            }
        }

        message_ids
    }

    /// `NotificationManager::get_call_notification_group_id` (cpp:2349-2389).
    pub fn get_call_notification_group_id(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        dialog_id: DialogId,
    ) -> NotificationGroupId {
        if let Some(&group_id) = self.dialog_id_to_call_notification_group_id.get(&dialog_id) {
            return group_id;
        }
        if !dialog_id.is_valid() {
            return NotificationGroupId::default();
        }

        if self.available_call_notification_group_ids.is_empty() {
            // need to reserve a new group_id for calls
            if self.call_notification_group_ids.len() >= MAX_CALL_NOTIFICATION_GROUPS {
                return NotificationGroupId::default();
            }
            let last_group_id = self
                .call_notification_group_ids
                .last()
                .copied()
                .unwrap_or_default();
            let mut next_notification_group_id;
            loop {
                next_notification_group_id = self.get_next_notification_group_id(env);
                if !next_notification_group_id.is_valid() {
                    return NotificationGroupId::default();
                }
                if last_group_id.get() < next_notification_group_id.get() {
                    break;
                }
            } // just in case

            self.call_notification_group_ids
                .push(next_notification_group_id);
            let call_notification_group_ids_string = self
                .call_notification_group_ids
                .iter()
                .map(|group_id| group_id.get().to_string())
                .collect::<Vec<_>>()
                .join(",");
            env.pmc_set(
                NOTIFICATION_CALL_GROUP_IDS_KEY,
                &call_notification_group_ids_string,
            );
            self.available_call_notification_group_ids
                .insert(next_notification_group_id);
        }

        let group_id = *self
            .available_call_notification_group_ids
            .iter()
            .next()
            .expect("non-empty");
        self.available_call_notification_group_ids.remove(&group_id);
        self.dialog_id_to_call_notification_group_id
            .insert(dialog_id, group_id);
        group_id
    }

    /// `NotificationManager::add_call_notification` (cpp:2391-2420); the
    /// `create_new_call_notification` factory is inlined as the
    /// `NotificationType::NewCall` variant.
    pub fn add_call_notification(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        dialog_id: DialogId,
        call_id: CallId,
    ) {
        assert!(dialog_id.is_valid());
        assert!(call_id.is_valid());
        if env.is_disabled() || self.max_notification_group_count == 0 {
            return;
        }

        let group_id = self.get_call_notification_group_id(env, dialog_id);
        if !group_id.is_valid() {
            return;
        }

        env.force_create_dialog(dialog_id, "add_call_notification", false);

        {
            let active_notifications = self.active_call_notifications.entry(dialog_id).or_default();
            if active_notifications.len() >= MAX_CALL_NOTIFICATIONS {
                return;
            }
        }

        let notification_id = self.get_next_notification_id(env);
        if !notification_id.is_valid() {
            return;
        }
        self.active_call_notifications
            .entry(dialog_id)
            .or_default()
            .push(ActiveCallNotification {
                call_id,
                notification_id,
            });

        self.add_notification(
            env,
            group_id,
            NotificationGroupType::Calls,
            dialog_id,
            env.unix_time() + 120,
            dialog_id,
            false,
            -1,
            0,
            notification_id,
            NotificationType::NewCall { call_id },
            "add_call_notification",
        );
    }

    /// `NotificationManager::remove_call_notification` (cpp:2422-2471).
    pub fn remove_call_notification(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        dialog_id: DialogId,
        call_id: CallId,
    ) {
        assert!(dialog_id.is_valid());
        assert!(call_id.is_valid());
        if env.is_disabled() || self.max_notification_group_count == 0 {
            return;
        }

        let Some(&group_id) = self.dialog_id_to_call_notification_group_id.get(&dialog_id) else {
            return;
        };
        assert!(group_id.is_valid());

        let notification_pos = {
            let active_notifications = self.active_call_notifications.entry(dialog_id).or_default();
            active_notifications
                .iter()
                .position(|active| active.call_id == call_id)
        };
        let Some(notification_pos) = notification_pos else {
            return;
        };
        let notification_id =
            self.active_call_notifications[&dialog_id][notification_pos].notification_id;

        self.remove_notification(
            env,
            group_id,
            notification_id,
            true,
            true,
            Promise::default(),
            "remove_call_notification",
        );

        let is_empty = {
            let active_notifications = self
                .active_call_notifications
                .get_mut(&dialog_id)
                .expect("the entry exists");
            active_notifications.remove(notification_pos);
            active_notifications.is_empty()
        };
        if !is_empty {
            return;
        }

        self.active_call_notifications.remove(&dialog_id);
        self.available_call_notification_group_ids.insert(group_id);
        self.dialog_id_to_call_notification_group_id
            .remove(&dialog_id);

        if !env.close_flag() {
            self.flush_pending_notifications_timeout
                .remove(&(group_id.get() as i64));
        }
        self.flush_pending_notifications(env, group_id);
        self.force_flush_pending_updates(env, group_id, "reuse call group_id");

        let group_key = self
            .get_group(group_id)
            .expect("the call notification group is present");
        assert_eq!(group_key.dialog_id, dialog_id);
        let group = self.groups.get(&group_key).expect("the group exists");
        assert_eq!(group_key.last_notification_date, 0);
        assert_eq!(group.total_count, 0);
        assert!(group.notifications.is_empty());
        assert!(group.pending_notifications.is_empty());
        assert_eq!(group.r#type, NotificationGroupType::Calls);
        assert!(!group.is_being_loaded_from_database);
        assert!(!self.pending_updates.contains_key(&group_id.get()));
        self.delete_group(group_key);
    }

    // ---- the two timeout families (the C++ MultiTimeout members) ----

    /// `flush_pending_notifications_timeout_.add_timeout_in` — keeps an
    /// existing deadline for the key.
    pub fn add_flush_pending_notifications_timeout(
        &mut self,
        env: &impl NotificationManagerEnv,
        key: i64,
        timeout_secs: f64,
    ) {
        self.flush_pending_notifications_timeout
            .entry(key)
            .or_insert(env.now() + timeout_secs);
    }

    /// `flush_pending_notifications_timeout_.set_timeout_in` — replaces the
    /// deadline for the key.
    pub fn set_flush_pending_notifications_timeout(
        &mut self,
        env: &impl NotificationManagerEnv,
        key: i64,
        timeout_secs: f64,
    ) {
        self.flush_pending_notifications_timeout
            .insert(key, env.now() + timeout_secs);
    }

    /// `flush_pending_updates_timeout_.add_timeout_in`.
    pub fn add_flush_pending_updates_timeout(
        &mut self,
        env: &impl NotificationManagerEnv,
        key: i32,
        timeout_secs: f64,
    ) {
        self.flush_pending_updates_timeout
            .entry(key)
            .or_insert(env.now() + timeout_secs);
    }

    /// `flush_pending_updates_timeout_.set_timeout_in`.
    pub fn set_flush_pending_updates_timeout(
        &mut self,
        env: &impl NotificationManagerEnv,
        key: i32,
        timeout_secs: f64,
    ) {
        self.flush_pending_updates_timeout
            .insert(key, env.now() + timeout_secs);
    }

    /// The earliest deadline across both timeout families — the Td timeout
    /// slot merge point (the M.7 `next_deadline` convention).
    pub fn next_deadline(&self) -> Option<f64> {
        self.flush_pending_notifications_timeout
            .values()
            .chain(self.flush_pending_updates_timeout.values())
            .copied()
            .reduce(f64::min)
    }

    // ---- M.13.7: get-difference hooks + the timeout pump ----

    /// `NotificationManager::before_get_difference` (cpp:4011-4021).
    pub fn before_get_difference(&mut self, env: &mut impl NotificationManagerEnv) {
        if env.is_disabled() {
            return;
        }
        if self.running_get_difference {
            return;
        }

        self.running_get_difference = true;
        self.on_unreceived_notification_update_count_changed(env, 1, 0, "before_get_difference");
    }

    /// `NotificationManager::after_get_difference` (cpp:4023-4034).
    pub fn after_get_difference(&mut self, env: &mut impl NotificationManagerEnv) {
        if env.is_disabled() {
            return;
        }

        assert!(self.running_get_difference);
        self.running_get_difference = false;
        self.on_unreceived_notification_update_count_changed(env, -1, 0, "after_get_difference");
        if !env.close_flag() {
            self.set_flush_pending_notifications_timeout(
                env,
                0,
                MIN_NOTIFICATION_DELAY_MS as f64 * 1e-3,
            );
        }
    }

    /// `NotificationManager::after_get_difference_impl` (cpp:4036-4057).
    pub fn after_get_difference_impl(&mut self, env: &mut impl NotificationManagerEnv) {
        if env.close_flag() || self.running_get_difference {
            return;
        }

        let mut to_remove_temporary_notifications_group_ids: Vec<NotificationGroupId> = Vec::new();
        for (group_key, group) in &self.groups {
            if !self
                .running_get_chat_difference
                .contains(&group_key.group_id.get())
                && Self::get_temporary_notification_total_count(group) > 0
            {
                to_remove_temporary_notifications_group_ids.push(group_key.group_id);
            }
        }
        for &group_id in to_remove_temporary_notifications_group_ids.iter().rev() {
            self.remove_temporary_notifications(env, group_id, "after_get_difference");
        }

        self.flush_all_pending_updates(env, false, "after_get_difference");
    }

    /// `NotificationManager::before_get_chat_difference` (cpp:4059-4069).
    pub fn before_get_chat_difference(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_id: NotificationGroupId,
    ) {
        if env.is_disabled() {
            return;
        }

        assert!(group_id.is_valid());
        if self.running_get_chat_difference.insert(group_id.get()) {
            self.on_unreceived_notification_update_count_changed(
                env,
                1,
                group_id.get(),
                "before_get_chat_difference",
            );
        }
    }

    /// `NotificationManager::after_get_chat_difference` (cpp:4071-4085).
    pub fn after_get_chat_difference(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_id: NotificationGroupId,
    ) {
        if env.is_disabled() {
            return;
        }

        assert!(group_id.is_valid());
        if self.running_get_chat_difference.remove(&group_id.get()) {
            if !env.close_flag() {
                self.set_flush_pending_notifications_timeout(
                    env,
                    -(group_id.get() as i64),
                    MIN_NOTIFICATION_DELAY_MS as f64 * 1e-3,
                );
            }
            self.on_unreceived_notification_update_count_changed(
                env,
                -1,
                group_id.get(),
                "after_get_chat_difference",
            );
        }
    }

    /// `NotificationManager::after_get_chat_difference_impl` (cpp:4087-4098).
    pub fn after_get_chat_difference_impl(
        &mut self,
        env: &mut impl NotificationManagerEnv,
        group_id: NotificationGroupId,
    ) {
        if env.close_flag() || self.running_get_chat_difference.contains(&group_id.get()) {
            return;
        }

        assert!(group_id.is_valid());
        if !self.running_get_difference && self.pending_updates.contains_key(&group_id.get()) {
            self.remove_temporary_notifications(env, group_id, "after_get_chat_difference");
            self.force_flush_pending_updates(env, group_id, "after_get_chat_difference");
        }
    }

    /// The collapsed `on_flush_pending_notifications_timeout_callback` +
    /// `on_flush_pending_updates_timeout_callback` (cpp:137-168) pump: fires
    /// every due deadline once. The notifications-timeout key convention is
    /// group_id > 0 → `flush_pending_notifications`, 0 →
    /// `after_get_difference_impl`, < 0 → `after_get_chat_difference_impl`;
    /// the updates timeout flushes that group's pending updates.
    pub fn tick_timeouts(&mut self, env: &mut impl NotificationManagerEnv) {
        let now = env.now();
        let due_notifications: Vec<i64> = self
            .flush_pending_notifications_timeout
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(&key, _)| key)
            .collect();
        let due_updates: Vec<i32> = self
            .flush_pending_updates_timeout
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(&key, _)| key)
            .collect();

        for key in due_notifications {
            self.flush_pending_notifications_timeout.remove(&key);
            if env.close_flag() {
                return;
            }
            if key > 0 {
                self.flush_pending_notifications(env, NotificationGroupId::new(key as i32));
            } else if key == 0 {
                self.after_get_difference_impl(env);
            } else {
                self.after_get_chat_difference_impl(env, NotificationGroupId::new((-key) as i32));
            }
        }
        for key in due_updates {
            self.flush_pending_updates_timeout.remove(&key);
            if env.close_flag() {
                return;
            }
            self.flush_pending_updates(env, key, "timeout");
        }
    }
}

/// A strict canonical-form pmc int32 parse (the C++ `to_integer<int32>` is
/// a lenient prefix parse; values are self-written, so intact databases
/// parse identically — see the module doc).
fn parse_pmc_i32(value: &str, key: &str) -> i32 {
    match value.parse::<i32>() {
        Ok(value) => value,
        Err(_) => {
            tdutils::log_error!("Failed to parse {key} value {value:?}");
            0
        }
    }
}

/// `NotificationManager::get_push_receiver_id` (NotificationManager.cpp) —
/// the sync `getPushReceiverId` payload walk: `"{}"` is id 0, the payload
/// must be a JSON object, the optional `"data"` sub-object replaces it,
/// and the receiver id comes from the `"p"` encrypted blob (the first
/// base64url 12 chars reinterpreted as the little-endian i64) or the
/// `"user_id"` field.
pub(crate) fn get_push_receiver_id(payload: &str) -> Result<i64, (i32, String)> {
    use tdutils::json::{JsonValue, JsonValueType, json_decode};

    if payload == "{}" {
        return Ok(0);
    }

    let json_value = match json_decode(payload) {
        tdutils::status::Result::Ok(value) => value,
        tdutils::status::Result::Err(status) => {
            return Err((400, status.message().to_string()));
        }
    };
    if json_value.ty() != JsonValueType::Object {
        return Err((400, "Expected JSON object".to_string()));
    }

    let mut data = match json_value {
        JsonValue::Object(object) => object,
        _ => unreachable!("the Object check above"),
    };
    if data.has_field("data") {
        let data_data = match data.extract_required_field("data", JsonValueType::Object) {
            tdutils::status::Result::Ok(value) => value,
            tdutils::status::Result::Err(status) => {
                return Err((400, status.message().to_string()));
            }
        };
        data = match data_data {
            JsonValue::Object(object) => object,
            _ => unreachable!("extract_required_field checked the type"),
        };
    }

    if data.has_field("p") {
        let encrypted_data = match data.get_required_string_field("p") {
            tdutils::status::Result::Ok(value) => value,
            tdutils::status::Result::Err(status) => {
                return Err((400, status.message().to_string()));
            }
        };
        if encrypted_data.len() < 12 {
            return Err((400, "Encrypted payload is too small".to_string()));
        }
        let decoded = match base64_decode_url(&encrypted_data[..12]) {
            Some(decoded) => decoded,
            None => return Err((400, "Failed to base64url-decode payload".to_string())),
        };
        debug_assert_eq!(decoded.len(), 9);
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&decoded[..8]);
        return Ok(i64::from_le_bytes(bytes));
    }
    if data.has_field("user_id") {
        let user_id = match data.get_required_long_field("user_id") {
            tdutils::status::Result::Ok(value) => value,
            tdutils::status::Result::Err(status) => {
                return Err((400, status.message().to_string()));
            }
        };
        if user_id <= 0 {
            return Err((400, format!("Receive wrong user_id {user_id}")));
        }
        return Ok(user_id);
    }

    Ok(0)
}

/// `td::base64url_decode` (base64.cpp) — the URL-safe alphabet subset the
/// push payload walks (12 chars → 9 bytes; any other length is an error
/// here because the caller pins the 12-char window).
fn base64_decode_url(input: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(input.as_bytes())
        .ok()
}

#[cfg(test)]
mod tests {
    /// 4c6377aa7 — an edit (updateNotification) of a removed notification
    /// no longer remembers the id as already-removed: the later
    /// removal inside a pending group update survives.
    #[test]
    fn edit_removal_keeps_the_later_group_removal() {
        let mut manager = NotificationManager::default();
        let notification = td_api::Notification {
            id: 5,
            date: 0,
            is_silent: false,
            r#type: None,
        };
        manager.pending_updates.insert(
            1,
            vec![
                Some(td_api::Update::updateNotification(
                    td_api::updateNotification {
                        notification_group_id: 1,
                        notification: Some(notification),
                    },
                )),
                Some(td_api::Update::updateNotificationGroup(
                    td_api::updateNotificationGroup {
                        notification_group_id: 1,
                        r#type: None,
                        chat_id: 0,
                        notification_settings_chat_id: 0,
                        notification_sound_id: 0,
                        total_count: 0,
                        added_notifications: Vec::new(),
                        removed_notification_ids: vec![5],
                    },
                )),
            ],
        );
        manager.remove_added_notifications_from_pending_updates(
            NotificationGroupId::new(1),
            |notification| notification.id == 5,
        );
        let updates = manager.pending_updates.get(&1).unwrap();
        assert!(updates[0].is_none(), "the edit update itself is dropped");
        let Some(td_api::Update::updateNotificationGroup(group)) = &updates[1] else {
            panic!("the group update stays");
        };
        assert_eq!(group.removed_notification_ids, vec![5]);
    }

    use super::*;
    use tdutils::promise::PromiseCreator;

    /// `NotificationManager::get_push_receiver_id` — the "{}"/user_id/"p"
    /// walks and the 400 bands (the sync getPushReceiverId arm).
    #[test]
    fn push_receiver_id_walks_the_payload_like_the_cpp() {
        use crate::notification_manager::get_push_receiver_id;

        assert_eq!(get_push_receiver_id("{}"), Ok(0));

        // The "data" sub-object replaces the top-level payload.
        assert_eq!(
            get_push_receiver_id(r#"{"data":{"user_id":42},"user_id":7}"#),
            Ok(42)
        );
        assert_eq!(get_push_receiver_id(r#"{"user_id":42}"#), Ok(42));

        // A "p" payload: 12 base64url chars decode to 9 bytes, the first
        // 8 little-endian bytes are the id. "AQAAAAAAAAAA" is
        // 01 00 00 00 00 00 00 00 00 — id 1.
        assert_eq!(get_push_receiver_id(r#"{"p":"AQAAAAAAAAAA"}"#), Ok(1));

        assert_eq!(
            get_push_receiver_id("not json"),
            Err((400, "Token starts with 'n' -- null expected".to_string()))
        );
        assert_eq!(
            get_push_receiver_id(r#"[1,2]"#),
            Err((400, "Expected JSON object".to_string()))
        );
        assert_eq!(
            get_push_receiver_id(r#"{"data":3}"#),
            Err((400, "Field \"data\" must be of type Object".to_string()))
        );
        assert_eq!(
            get_push_receiver_id(r#"{"p":"short"}"#),
            Err((400, "Encrypted payload is too small".to_string()))
        );
        assert_eq!(
            get_push_receiver_id(r#"{"user_id":0}"#),
            Err((400, "Receive wrong user_id 0".to_string()))
        );
        assert_eq!(get_push_receiver_id(r#"{"c":"x"}"#), Ok(0));
    }

    struct TestEnv {
        disabled: bool,
        close_flag: bool,
        options: HashMap<String, i64>,
        bool_options: HashMap<String, bool>,
        pmc: HashMap<String, String>,
        time: f64,
        unix_time: i32,
        updates: Vec<td_api::Update>,
        message_loaded: bool,
        db_groups: HashMap<NotificationGroupId, NotificationGroupFromDatabase>,
        my_online_status: MyOnlineStatus,
        db_group_keys: Vec<NotificationGroupKey>,
        db_notifications: Vec<Notification>,
        db_error: bool,
        use_message_db: bool,
        force_created_dialogs: Vec<DialogId>,
        /// the `set_contact_option_boolean` writes (the contact-sign-up band).
        bool_writes: Vec<(String, bool)>,
        /// the `set_contact_option_empty` writes.
        empty_writes: Vec<String>,
    }

    impl TestEnv {
        fn new() -> TestEnv {
            TestEnv {
                disabled: false,
                close_flag: false,
                options: HashMap::new(),
                bool_options: HashMap::new(),
                pmc: HashMap::new(),
                time: 0.0,
                unix_time: 1_000_000,
                updates: Vec::new(),
                message_loaded: true,
                db_groups: HashMap::new(),
                my_online_status: MyOnlineStatus::default(),
                db_group_keys: Vec::new(),
                db_notifications: Vec::new(),
                db_error: false,
                use_message_db: true,
                force_created_dialogs: Vec::new(),
                bool_writes: Vec::new(),
                empty_writes: Vec::new(),
            }
        }
    }

    impl NotificationTypeEnv for TestEnv {
        fn get_message_object(
            &self,
            _dialog_id: DialogId,
            _message_id: MessageId,
        ) -> Option<td_api::Message> {
            if self.message_loaded {
                Some(td_api::Message::default())
            } else {
                None
            }
        }
    }

    impl NotificationManagerEnv for TestEnv {
        fn is_disabled(&self) -> bool {
            self.disabled
        }

        fn set_contact_option_boolean(&mut self, name: &str, value: bool) {
            self.bool_writes.push((name.to_owned(), value));
            self.bool_options.insert(name.to_owned(), value);
        }

        fn set_contact_option_empty(&mut self, name: &str) {
            self.empty_writes.push(name.to_owned());
            self.bool_options.remove(name);
        }

        fn close_flag(&self) -> bool {
            self.close_flag
        }

        fn get_option_integer(&self, name: &str, default_value: i64) -> i64 {
            self.options.get(name).copied().unwrap_or(default_value)
        }

        fn get_option_boolean(&self, name: &str, default_value: bool) -> bool {
            self.bool_options
                .get(name)
                .copied()
                .unwrap_or(default_value)
        }

        fn unix_time(&self) -> i32 {
            self.unix_time
        }

        fn now(&self) -> f64 {
            self.time
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn pmc_get(&mut self, key: &str) -> String {
            self.pmc.get(key).cloned().unwrap_or_default()
        }

        fn pmc_set(&mut self, key: &str, value: &str) {
            self.pmc.insert(key.to_string(), value.to_string());
        }

        fn pmc_erase(&mut self, key: &str) {
            self.pmc.remove(key);
        }

        fn get_message_notification_group_force(
            &self,
            group_id: NotificationGroupId,
        ) -> Option<NotificationGroupFromDatabase> {
            self.db_groups.get(&group_id).cloned()
        }

        fn get_my_online_status(&self) -> MyOnlineStatus {
            self.my_online_status
        }

        fn use_message_database(&self) -> bool {
            self.use_message_db
        }

        fn get_message_notification_group_keys_from_database(
            &self,
            from_group_key: NotificationGroupKey,
            limit: i32,
        ) -> Vec<NotificationGroupKey> {
            self.db_group_keys
                .iter()
                .copied()
                .filter(|group_key| from_group_key < *group_key)
                .take(limit as usize)
                .collect()
        }

        fn get_message_notifications_from_database(
            &self,
            _dialog_id: DialogId,
            _group_id: NotificationGroupId,
            _from_notification_id: NotificationId,
            _from_message_id: MessageId,
            _limit: i32,
        ) -> Result<Vec<Notification>, Status> {
            if self.db_error {
                Err(Status::error_message("db error"))
            } else {
                Ok(self.db_notifications.clone())
            }
        }

        fn force_create_dialog(&mut self, dialog_id: DialogId, _source: &str, _force: bool) {
            self.force_created_dialogs.push(dialog_id);
        }
    }

    fn update_notification_group(
        group_id: i32,
        added: Vec<(i32, Option<td_api::NotificationType>)>,
        removed: Vec<i32>,
        total_count: i32,
    ) -> td_api::updateNotificationGroup {
        td_api::updateNotificationGroup {
            notification_group_id: group_id,
            r#type: Some(NotificationGroupType::Messages.get_notification_group_type_object()),
            chat_id: 1,
            notification_settings_chat_id: 1,
            notification_sound_id: 0,
            total_count,
            added_notifications: added
                .into_iter()
                .map(|(id, type_)| td_api::Notification {
                    id,
                    date: id,
                    is_silent: false,
                    r#type: type_,
                })
                .collect(),
            removed_notification_ids: removed,
        }
    }

    fn notification_type(_seed: i32) -> Option<td_api::NotificationType> {
        Some(td_api::NotificationType::notificationTypeNewSecretChat(
            td_api::notificationTypeNewSecretChat,
        ))
    }

    fn sent_group_updates(updates: &[td_api::Update]) -> Vec<&td_api::updateNotificationGroup> {
        updates
            .iter()
            .filter_map(|update| match update {
                td_api::Update::updateNotificationGroup(update) => Some(update),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn init_defaults_and_id_load() {
        let mut manager = NotificationManager::default();
        let mut env = TestEnv::new();
        env.pmc
            .insert(NOTIFICATION_ID_CURRENT_KEY.to_string(), "123".to_string());
        env.pmc.insert(
            NOTIFICATION_GROUP_ID_CURRENT_KEY.to_string(),
            "7".to_string(),
        );
        env.options
            .insert("notification_group_count_max".to_string(), 3);

        manager.init(&mut env);

        assert!(manager.is_inited);
        assert_eq!(manager.current_notification_id, NotificationId::new(123));
        assert_eq!(
            manager.current_notification_group_id,
            NotificationGroupId::new(7)
        );
        assert_eq!(manager.max_notification_group_count, 3);
        assert_eq!(manager.max_notification_group_size, 10);
        assert_eq!(manager.keep_notification_group_size, 20);
        assert_eq!(
            manager.online_cloud_timeout_ms,
            DEFAULT_ONLINE_CLOUD_TIMEOUT_MS
        );
        assert_eq!(
            manager.notification_cloud_delay_ms,
            DEFAULT_ONLINE_CLOUD_DELAY_MS
        );
        assert_eq!(
            manager.notification_default_delay_ms,
            DEFAULT_DEFAULT_DELAY_MS
        );
        // the empty-database load walk resets the cursor to date 0
        // (the C++ do-while exits when nothing was loaded)
        assert_eq!(
            manager
                .last_loaded_notification_group_key
                .last_notification_date,
            0
        );
        // max_notification_group_count > 0 but the db layer is blocked:
        // nothing was loaded
        assert!(manager.groups.is_empty());
        // no updates sent without binlog processing
        assert!(env.updates.is_empty());

        // a second init is a no-op
        env.pmc
            .insert(NOTIFICATION_ID_CURRENT_KEY.to_string(), "999".to_string());
        manager.init(&mut env);
        assert_eq!(manager.current_notification_id, NotificationId::new(123));
    }

    #[test]
    fn init_loads_call_group_ids_and_announcements() {
        let mut manager = NotificationManager::default();
        let mut env = TestEnv::new();
        // keep the seeded announcement dates inside the cache window
        env.unix_time = 1_000;
        env.pmc.insert(
            NOTIFICATION_CALL_GROUP_IDS_KEY.to_string(),
            "5,0,12".to_string(),
        );
        env.pmc.insert(
            NOTIFICATION_ANNOUNCEMENT_IDS_KEY.to_string(),
            "11,100,0,50,13,20".to_string(),
        );

        manager.init(&mut env);

        assert_eq!(
            manager.call_notification_group_ids,
            vec![NotificationGroupId::new(5), NotificationGroupId::new(12)]
        );
        assert!(
            manager
                .available_call_notification_group_ids
                .contains(&NotificationGroupId::new(5))
        );
        assert!(
            manager
                .available_call_notification_group_ids
                .contains(&NotificationGroupId::new(12))
        );
        assert_eq!(manager.announcement_id_date.get(&11), Some(&100));
        assert_eq!(manager.announcement_id_date.get(&13), Some(&20));
        assert!(!manager.announcement_id_date.contains_key(&0));

        // the announcement cache is rewritten without the dropped entries
        let saved = env.pmc.get(NOTIFICATION_ANNOUNCEMENT_IDS_KEY).unwrap();
        let saved_ids: Vec<i32> = saved.split(',').map(|part| part.parse().unwrap()).collect();
        assert!(saved_ids.contains(&11));
        assert!(saved_ids.contains(&13));
        assert!(!saved_ids.contains(&0));
    }

    #[test]
    fn announcement_cache_expiry_erases_the_key() {
        let mut manager = NotificationManager::default();
        let mut env = TestEnv::new();
        env.pmc.insert(
            NOTIFICATION_ANNOUNCEMENT_IDS_KEY.to_string(),
            "1,10".to_string(),
        );
        env.unix_time = 10 + ANNOUNCEMENT_ID_CACHE_TIME + 1;

        manager.init(&mut env);
        assert!(manager.announcement_id_date.is_empty());
        assert!(!env.pmc.contains_key(NOTIFICATION_ANNOUNCEMENT_IDS_KEY));
    }

    #[test]
    fn get_next_ids_persist() {
        let mut manager = NotificationManager::default();
        let mut env = TestEnv::new();
        manager.current_notification_id = NotificationId::new(5);
        manager.current_notification_group_id = NotificationGroupId::new(9);

        assert_eq!(
            manager.get_next_notification_id(&mut env),
            NotificationId::new(6)
        );
        assert_eq!(
            manager.get_next_notification_group_id(&mut env),
            NotificationGroupId::new(10)
        );
        assert_eq!(env.pmc.get(NOTIFICATION_ID_CURRENT_KEY).unwrap(), "6");
        assert_eq!(
            env.pmc.get(NOTIFICATION_GROUP_ID_CURRENT_KEY).unwrap(),
            "10"
        );

        // disabled: no allocation
        env.disabled = true;
        assert_eq!(
            manager.get_next_notification_id(&mut env),
            NotificationId::default()
        );
        assert_eq!(
            manager.get_next_notification_group_id(&mut env),
            NotificationGroupId::default()
        );
    }

    #[test]
    fn update_diff_add_remove_add_merges() {
        let mut manager = NotificationManager {
            max_notification_group_size: 10,
            keep_notification_group_size: 20,
            is_inited: true,
            is_binlog_processed: true,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        // an empty group so the group key exists for flush_pending_updates
        manager.add_group(
            NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(1), 0),
            NotificationGroup::default(),
            "test",
        );

        manager.add_update_notification_group(
            &mut env,
            update_notification_group(1, vec![(1, notification_type(1))], vec![], 1),
        );
        manager.add_update_notification_group(
            &mut env,
            update_notification_group(1, vec![], vec![1], 1),
        );
        manager.add_update_notification_group(
            &mut env,
            update_notification_group(1, vec![(1, notification_type(2))], vec![], 2),
        );
        assert_eq!(manager.pending_updates.get(&1).map_or(0, Vec::len), 3);
        assert_eq!(manager.delayed_notification_update_count, 1);
        assert_eq!(manager.flush_pending_updates_timeout.len(), 1);

        manager.flush_pending_updates(&mut env, 1, "test");

        assert!(!manager.pending_updates.contains_key(&1));
        assert_eq!(manager.delayed_notification_update_count, 0);

        // the C++ diff for add → remove → re-add collapses the sequence:
        // the deletion is dropped (the re-add merges its type into the
        // first addition), the emptied removal update hands its
        // total_count to the survivor, and the emptied re-add hands its
        // own total_count back — the survivor ends with the re-add's
        // total count
        let group_updates = sent_group_updates(&env.updates);
        assert_eq!(group_updates.len(), 1);
        assert_eq!(group_updates[0].added_notifications.len(), 1);
        assert_eq!(group_updates[0].added_notifications[0].id, 1);
        assert_eq!(group_updates[0].removed_notification_ids, Vec::<i32>::new());
        assert_eq!(group_updates[0].total_count, 2);

        // the scheduled flush timeout stays queued until it fires
        assert!(!manager.flush_pending_updates_timeout.is_empty());
        env.time += 1.0;
        manager.tick_timeouts(&mut env);
        assert!(manager.flush_pending_updates_timeout.is_empty());
    }

    #[test]
    fn update_diff_edits_merge_into_additions() {
        let mut manager = NotificationManager {
            max_notification_group_size: 10,
            keep_notification_group_size: 20,
            is_inited: true,
            is_binlog_processed: true,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        manager.add_group(
            NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(1), 0),
            NotificationGroup::default(),
            "test",
        );

        manager.add_update_notification_group(
            &mut env,
            update_notification_group(1, vec![(1, notification_type(1))], vec![], 1),
        );
        manager.add_update_notification(
            &mut env,
            NotificationGroupId::new(1),
            DialogId::new(1),
            &Notification::new(
                NotificationId::new(1),
                1,
                false,
                NotificationType::new_secret_chat(),
            ),
        );

        manager.flush_pending_updates(&mut env, 1, "test");

        let group_updates = sent_group_updates(&env.updates);
        // add + edit → a single addition with the edited type
        assert_eq!(group_updates.len(), 1);
        assert_eq!(group_updates[0].added_notifications.len(), 1);
        assert_eq!(group_updates[0].added_notifications[0].id, 1);
    }

    #[test]
    fn update_diff_add_edit_remove_collapses() {
        let mut manager = NotificationManager {
            max_notification_group_size: 10,
            keep_notification_group_size: 20,
            is_inited: true,
            is_binlog_processed: true,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        manager.add_group(
            NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(1), 0),
            NotificationGroup::default(),
            "test",
        );

        // add, edit, remove — the final state is "removed", so the addition
        // (and with it the edit and the deletion) is unneeded
        manager.add_update_notification_group(
            &mut env,
            update_notification_group(1, vec![(1, notification_type(1))], vec![], 1),
        );
        manager.add_update_notification(
            &mut env,
            NotificationGroupId::new(1),
            DialogId::new(1),
            &Notification::new(
                NotificationId::new(1),
                1,
                false,
                NotificationType::new_secret_chat(),
            ),
        );
        manager.add_update_notification_group(
            &mut env,
            update_notification_group(1, vec![], vec![1], 0),
        );

        manager.flush_pending_updates(&mut env, 1, "test");

        // the secret-chat edit is not delayable: add_update_notification
        // force-flushes [add, edit] immediately (the edit merges into the
        // addition), and the removal is flushed by the test's explicit
        // flush_pending_updates afterwards
        let group_updates = sent_group_updates(&env.updates);
        assert_eq!(group_updates.len(), 2);
        assert_eq!(group_updates[0].added_notifications.len(), 1);
        assert_eq!(group_updates[0].added_notifications[0].id, 1);
        assert_eq!(group_updates[0].total_count, 1);
        assert_eq!(group_updates[1].removed_notification_ids, vec![1]);
        assert_eq!(group_updates[1].total_count, 0);
    }

    #[test]
    fn update_diff_keeps_lone_removal() {
        let mut manager = NotificationManager {
            max_notification_group_size: 10,
            keep_notification_group_size: 20,
            is_inited: true,
            is_binlog_processed: true,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        manager.add_group(
            NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(1), 0),
            NotificationGroup::default(),
            "test",
        );

        // a removal without a prior addition must be kept
        manager.add_update_notification_group(
            &mut env,
            update_notification_group(1, vec![], vec![1], 0),
        );

        manager.flush_pending_updates(&mut env, 1, "test");

        let group_updates = sent_group_updates(&env.updates);
        assert_eq!(group_updates.len(), 1);
        assert_eq!(group_updates[0].removed_notification_ids, vec![1]);
        assert!(group_updates[0].added_notifications.is_empty());
    }

    #[test]
    fn update_diff_moves_deletions_to_first_group_update() {
        let mut manager = NotificationManager {
            max_notification_group_size: 10,
            keep_notification_group_size: 20,
            is_inited: true,
            is_binlog_processed: true,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        manager.add_group(
            NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(1), 0),
            NotificationGroup::default(),
            "test",
        );

        // two group updates; the second carries an unneeded deletion, which
        // moves to the first updateNotificationGroup; the updates then
        // combine into one
        manager.add_update_notification_group(
            &mut env,
            update_notification_group(1, vec![(1, notification_type(1))], vec![], 1),
        );
        manager.add_update_notification_group(
            &mut env,
            update_notification_group(1, vec![(2, notification_type(2))], vec![3], 2),
        );

        manager.flush_pending_updates(&mut env, 1, "test");

        let group_updates = sent_group_updates(&env.updates);
        assert_eq!(group_updates.len(), 1);
        assert_eq!(group_updates[0].added_notifications.len(), 2);
        assert_eq!(group_updates[0].added_notifications[0].id, 1);
        assert_eq!(group_updates[0].added_notifications[1].id, 2);
        assert_eq!(group_updates[0].removed_notification_ids, vec![3]);
        assert_eq!(group_updates[0].total_count, 2);
    }

    #[test]
    fn do_flush_applies_group_size_limits() {
        let mut manager = NotificationManager {
            max_notification_group_size: 2,
            keep_notification_group_size: 2,
            is_inited: true,
            is_binlog_processed: true,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        let group_key = NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(1), 5);
        let mut group = NotificationGroup {
            r#type: NotificationGroupType::Messages,
            total_count: 1,
            ..NotificationGroup::default()
        };
        group.notifications.push(Notification::new(
            NotificationId::new(1),
            1,
            false,
            NotificationType::new_secret_chat(),
        ));
        // the flush tail re-looks the group up (on_notification_processed)
        manager.add_group(group_key, group.clone(), "test");

        let mut pending = vec![
            PendingNotification {
                date: 2,
                settings_dialog_id: DialogId::new(1),
                disable_notification: false,
                ringtone_id: -1,
                notification_id: NotificationId::new(2),
                r#type: NotificationType::new_secret_chat(),
            },
            PendingNotification {
                date: 3,
                settings_dialog_id: DialogId::new(1),
                disable_notification: false,
                ringtone_id: -1,
                notification_id: NotificationId::new(3),
                r#type: NotificationType::new_secret_chat(),
            },
            PendingNotification {
                date: 4,
                settings_dialog_id: DialogId::new(1),
                disable_notification: false,
                ringtone_id: -1,
                notification_id: NotificationId::new(4),
                r#type: NotificationType::new_secret_chat(),
            },
        ];

        let force =
            manager.do_flush_pending_notifications(&mut env, group_key, &mut group, &mut pending);
        // the notification was already shown, so the pending are kept
        // (3 - 2 = 1 removed)
        assert_eq!(group.total_count, 4);
        assert_eq!(group.notifications.len(), 4);
        assert!(pending.is_empty());

        // secret-chat notifications cannot be delayed → the flush forces
        assert!(force);
        manager.force_flush_pending_updates(&mut env, NotificationGroupId::new(1), "test");
        let sent = sent_group_updates(&env.updates);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].added_notifications.len(), 2);
        assert_eq!(sent[0].removed_notification_ids, vec![1]);
        assert_eq!(sent[0].total_count, 4);
    }

    #[test]
    fn update_active_notifications_respects_count_and_size() {
        let mut manager = NotificationManager {
            max_notification_group_count: 2,
            max_notification_group_size: 1,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        let mut older = NotificationGroup {
            r#type: NotificationGroupType::Messages,
            total_count: 1,
            ..NotificationGroup::default()
        };
        // message notifications: the unloaded-message cull below needs
        // types that carry a message object
        older
            .notifications
            .push(message_notification(1, 10, 1 << 20));
        manager.add_group(
            NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(1), 10),
            older,
            "test",
        );

        let mut newer = NotificationGroup {
            r#type: NotificationGroupType::Mentions,
            total_count: 2,
            ..NotificationGroup::default()
        };
        newer
            .notifications
            .push(message_notification(2, 20, 2 << 20));
        newer
            .notifications
            .push(message_notification(3, 20, 3 << 20));
        manager.add_group(
            NotificationGroupKey::new(NotificationGroupId::new(2), DialogId::new(2), 20),
            newer,
            "test",
        );

        let update = manager.get_update_active_notifications(&env);
        // the newest group first, one notification per group (size cap)
        assert_eq!(update.groups.len(), 2);
        assert_eq!(update.groups[0].id, 2);
        assert_eq!(update.groups[0].notifications.len(), 1);
        assert_eq!(update.groups[0].notifications[0].id, 3);
        assert_eq!(update.groups[1].id, 1);
        assert_eq!(update.groups[1].notifications.len(), 1);

        // an unloaded message culls the notification and the group
        env.message_loaded = false;
        let update = manager.get_update_active_notifications(&env);
        assert!(update.groups.is_empty());
    }

    #[test]
    fn try_reuse_deletes_the_empty_group() {
        let mut manager = NotificationManager {
            current_notification_group_id: NotificationGroupId::new(5),
            max_notification_group_size: 10,
            keep_notification_group_size: 10,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        manager.add_group(
            NotificationGroupKey::new(NotificationGroupId::new(5), DialogId::new(1), 0),
            NotificationGroup::default(),
            "test",
        );

        manager.try_reuse_notification_group_id(&mut env, NotificationGroupId::new(5));
        assert!(manager.groups.is_empty());
        assert!(manager.group_keys.is_empty());
        assert_eq!(
            manager.current_notification_group_id,
            NotificationGroupId::new(4)
        );
        assert_eq!(env.pmc.get(NOTIFICATION_GROUP_ID_CURRENT_KEY).unwrap(), "4");
    }

    #[test]
    fn timeouts_keep_or_replace_deadlines() {
        let mut manager = NotificationManager::default();
        let mut env = TestEnv::new();
        env.time = 100.0;

        manager.add_flush_pending_updates_timeout(&env, 1, 0.05);
        assert_eq!(manager.flush_pending_updates_timeout.get(&1), Some(&100.05));
        env.time = 101.0;
        // add keeps the earlier deadline
        manager.add_flush_pending_updates_timeout(&env, 1, 10.0);
        assert_eq!(manager.flush_pending_updates_timeout.get(&1), Some(&100.05));
        // set replaces it
        manager.set_flush_pending_updates_timeout(&env, 1, 1.0);
        assert_eq!(manager.flush_pending_updates_timeout.get(&1), Some(&102.0));

        manager.add_flush_pending_notifications_timeout(&env, -1, 0.5);
        assert_eq!(manager.next_deadline(), Some(101.5));
    }

    #[test]
    fn get_last_updated_group_key_walks_the_count() {
        let mut manager = NotificationManager {
            max_notification_group_count: 2,
            ..NotificationManager::default()
        };
        // add_group rejects empty groups with a non-zero date (the C++
        // LOG_CHECK), so each group carries one notification
        manager.add_group(
            NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(1), 30),
            NotificationGroup {
                total_count: 1,
                r#type: NotificationGroupType::Messages,
                notifications: vec![message_notification(1, 30, 1 << 20)],
                ..NotificationGroup::default()
            },
            "test",
        );
        manager.add_group(
            NotificationGroupKey::new(NotificationGroupId::new(2), DialogId::new(2), 20),
            NotificationGroup {
                total_count: 1,
                r#type: NotificationGroupType::Messages,
                notifications: vec![message_notification(2, 20, 2 << 20)],
                ..NotificationGroup::default()
            },
            "test",
        );
        manager.add_group(
            NotificationGroupKey::new(NotificationGroupId::new(3), DialogId::new(3), 10),
            NotificationGroup {
                total_count: 1,
                r#type: NotificationGroupType::Messages,
                notifications: vec![message_notification(3, 10, 3 << 20)],
                ..NotificationGroup::default()
            },
            "test",
        );

        assert_eq!(
            manager.get_last_updated_group_key().group_id,
            NotificationGroupId::new(2)
        );
    }

    #[test]
    fn contact_registered_sync_state_round_trips() {
        let mut manager = NotificationManager {
            disable_contact_registered_notifications: true,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        manager.set_contact_registered_notifications_sync_state(&mut env, SyncState::Completed);
        assert_eq!(env.pmc.get(CONTACT_REGISTERED_SYNC_KEY).unwrap(), "21");
        assert_eq!(
            manager.contact_registered_notifications_sync_state,
            SyncState::Completed
        );
    }

    #[test]
    fn contact_sign_up_sync_machine_bands() {
        // the NotSynced + enabled band completes without a query
        // (cpp:2698-2701)
        let mut manager = NotificationManager::default();
        let mut env = TestEnv::new();
        manager.run_contact_registered_notifications_sync(&mut env);
        assert_eq!(
            manager.contact_registered_notifications_sync_state,
            SyncState::Completed,
            "the enabled NotSynced band completes locally"
        );
        assert_eq!(env.pmc.get(CONTACT_REGISTERED_SYNC_KEY).unwrap(), "20");

        // the disabled band arms the Pending state + the wire send
        // (cpp:2702-2714) — the net-inert posture parks at Pending
        let mut manager = NotificationManager {
            disable_contact_registered_notifications: true,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();
        manager.run_contact_registered_notifications_sync(&mut env);
        assert_eq!(
            manager.contact_registered_notifications_sync_state,
            SyncState::Pending
        );
        assert_eq!(env.pmc.get(CONTACT_REGISTERED_SYNC_KEY).unwrap(), "11");

        // the ok answer completes the machine (cpp:2721-2722)
        manager.on_contact_registered_notifications_sync(&mut env, true, Ok(()));
        assert_eq!(
            manager.contact_registered_notifications_sync_state,
            SyncState::Completed
        );

        // the error answer resends forever — the state re-arms Pending
        // (cpp:2722-2724)
        let mut manager = NotificationManager {
            disable_contact_registered_notifications: true,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();
        manager.run_contact_registered_notifications_sync(&mut env);
        manager.on_contact_registered_notifications_sync(
            &mut env,
            true,
            Err(Status::error(400, "CHANNEL_PRIVATE")),
        );
        assert_eq!(
            manager.contact_registered_notifications_sync_state,
            SyncState::Pending,
            "the resend-forever arm re-arms the wire query"
        );

        // a changed flag re-runs the sync with the fresh capture
        // (cpp:2718-2720)
        let mut manager = NotificationManager {
            disable_contact_registered_notifications: false,
            ..NotificationManager::default()
        };
        manager.contact_registered_notifications_sync_state = SyncState::Pending;
        let mut env = TestEnv::new();
        manager.on_contact_registered_notifications_sync(&mut env, true, Ok(()));
        assert_eq!(
            manager.contact_registered_notifications_sync_state,
            SyncState::Pending,
            "the changed flag re-arms instead of completing"
        );
    }

    #[test]
    fn contact_sign_up_get_family_parks_and_flips() {
        // the request parks FIFO + the wire send arms (cpp:2729-2744)
        let mut manager = NotificationManager::default();
        let mut env = TestEnv::new();
        manager.get_disable_contact_registered_notifications(&mut env, 11);
        manager.get_disable_contact_registered_notifications(&mut env, 12);
        assert_eq!(
            manager.pending_contact_sign_up_get_options,
            vec![11, 12],
            "FIFO"
        );

        // the disabled band parks nothing
        env.disabled = true;
        manager.get_disable_contact_registered_notifications(&mut env, 13);
        assert_eq!(manager.pending_contact_sign_up_get_options, vec![11, 12]);
        env.disabled = false;

        // the Bool answer flips the flag + writes the option
        // (cpp:2667-2678)
        let answered = manager.on_get_disable_contact_registered_notifications(&mut env, true);
        assert_eq!(answered, 11, "the head request pops");
        assert!(manager.disable_contact_registered_notifications);
        assert_eq!(
            env.bool_writes
                .last()
                .map(|(name, value)| (name.as_str(), *value)),
            Some(("disable_contact_registered_notifications", true))
        );

        // the unchanged-flag band answers without the write
        let answered = manager.on_get_disable_contact_registered_notifications(&mut env, true);
        assert_eq!(answered, 12);
        assert_eq!(env.bool_writes.len(), 1, "no second write");

        // the false answer clears the option (cpp:2673-2676)
        manager.get_disable_contact_registered_notifications(&mut env, 14);
        manager.on_get_disable_contact_registered_notifications(&mut env, false);
        assert!(!manager.disable_contact_registered_notifications);
        assert_eq!(
            env.empty_writes.last().map(String::as_str),
            Some("disable_contact_registered_notifications")
        );
    }

    // ---- M.13.2 / M.13.3 ----

    fn secret_chat_notification(id: i32, date: i32) -> Notification {
        Notification::new(
            NotificationId::new(id),
            date,
            false,
            NotificationType::new_secret_chat(),
        )
    }

    fn message_notification(id: i32, date: i32, message_id: i64) -> Notification {
        Notification::new(
            NotificationId::new(id),
            date,
            false,
            NotificationType::new_message(MessageId::new(message_id), true),
        )
    }

    fn pending(id: i32, date: i32, type_: NotificationType) -> PendingNotification {
        PendingNotification {
            date,
            settings_dialog_id: DialogId::new(1),
            disable_notification: false,
            ringtone_id: -1,
            notification_id: NotificationId::new(id),
            r#type: type_,
        }
    }

    #[test]
    fn first_and_last_accessors() {
        let mut group = NotificationGroup::default();
        assert_eq!(
            NotificationManager::get_first_notification_id(&group),
            NotificationId::default()
        );
        assert_eq!(
            NotificationManager::get_last_notification_id(&group),
            NotificationId::default()
        );
        assert_eq!(
            NotificationManager::get_first_object_id(&group),
            NotificationObjectId::default()
        );

        group.notifications.push(secret_chat_notification(1, 10));
        group.notifications.push(message_notification(2, 20, 100));

        assert_eq!(
            NotificationManager::get_first_notification_id(&group),
            NotificationId::new(1)
        );
        assert_eq!(
            NotificationManager::get_last_notification_id(&group),
            NotificationId::new(2)
        );
        assert_eq!(
            NotificationManager::get_first_object_id(&group),
            NotificationObjectId::default()
        );
        assert_eq!(
            NotificationManager::get_last_object_id(&group),
            NotificationObjectId::from_message_id(MessageId::new(100))
        );

        group.pending_notifications.push(pending(
            3,
            30,
            NotificationType::new_message(MessageId::new(101), false),
        ));
        assert_eq!(
            NotificationManager::get_last_notification_id(&group),
            NotificationId::new(3)
        );
        assert_eq!(
            NotificationManager::get_last_object_id(&group),
            NotificationObjectId::from_message_id(MessageId::new(101))
        );
        assert_eq!(
            NotificationManager::get_last_object_id_by_notification_id(
                &group,
                NotificationId::new(3)
            ),
            NotificationObjectId::from_message_id(MessageId::new(101))
        );
        assert_eq!(
            NotificationManager::get_last_object_id_by_notification_id(
                &group,
                NotificationId::new(2)
            ),
            NotificationObjectId::from_message_id(MessageId::new(100))
        );
        // ids up to 1 hold no valid object id: the secret chat has none
        assert_eq!(
            NotificationManager::get_last_object_id_by_notification_id(
                &group,
                NotificationId::new(1)
            ),
            NotificationObjectId::default()
        );
    }

    #[test]
    fn notification_delay_model() {
        let manager = NotificationManager {
            online_cloud_timeout_ms: DEFAULT_ONLINE_CLOUD_TIMEOUT_MS,
            notification_cloud_delay_ms: DEFAULT_ONLINE_CLOUD_DELAY_MS,
            notification_default_delay_ms: DEFAULT_DEFAULT_DELAY_MS,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();
        env.unix_time = 1_000;
        let delayed = pending(
            1,
            999,
            NotificationType::new_message(MessageId::new(1 << 20), true),
        );

        // secret chats never delay
        assert_eq!(
            manager.get_notification_delay_ms(
                &env,
                DialogId::new(-2_000_000_000_001),
                &delayed,
                100
            ),
            MIN_NOTIFICATION_DELAY_MS
        );
        // non-delayable types never delay
        let immediate = pending(1, 999, NotificationType::new_secret_chat());
        assert_eq!(
            manager.get_notification_delay_ms(&env, DialogId::new(1), &immediate, 100),
            MIN_NOTIFICATION_DELAY_MS
        );

        // fully offline: no delay, and the passed time (date 999, server
        // 1000 → 0 seconds) does not matter
        assert_eq!(
            manager.get_notification_delay_ms(&env, DialogId::new(1), &delayed, 0),
            MIN_NOTIFICATION_DELAY_MS
        );

        // some other client online (we are online locally too): the default
        // delay minus the passed time
        env.my_online_status.is_online_local = true;
        env.my_online_status.is_online_remote = true;
        assert_eq!(
            manager.get_notification_delay_ms(&env, DialogId::new(1), &delayed, 0),
            DEFAULT_DEFAULT_DELAY_MS
        );
        let delayed_old = pending(
            1,
            100,
            NotificationType::new_message(MessageId::new(1 << 20), true),
        );
        // passed = (1000 - 100 - 1) * 1000 = 899000 ms > delay → clamped to the minimum
        assert_eq!(
            manager.get_notification_delay_ms(&env, DialogId::new(1), &delayed_old, 0),
            MIN_NOTIFICATION_DELAY_MS
        );

        // offline locally but online remotely: the cloud delay
        env.my_online_status.is_online_remote = true;
        env.my_online_status.is_online_local = false;
        assert_eq!(
            manager.get_notification_delay_ms(&env, DialogId::new(1), &delayed, 0),
            DEFAULT_ONLINE_CLOUD_DELAY_MS
        );

        // recently was online remotely within the cloud timeout: cloud delay
        env.my_online_status.is_online_remote = false;
        env.my_online_status.was_online_remote = 900.0;
        env.my_online_status.was_online_local = 800.0;
        assert_eq!(
            manager.get_notification_delay_ms(&env, DialogId::new(1), &delayed, 0),
            DEFAULT_ONLINE_CLOUD_DELAY_MS
        );

        // the minimum delay wins over everything
        assert_eq!(
            manager.get_notification_delay_ms(&env, DialogId::new(1), &delayed, 60_000),
            60_000
        );
    }

    #[test]
    fn add_notification_intake() {
        let mut manager = NotificationManager {
            max_notification_group_count: 5,
            max_notification_group_size: 10,
            keep_notification_group_size: 20,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();
        env.time = 10.0;
        env.unix_time = 1_000;

        // a fresh group gets its type from the first notification
        manager.add_notification(
            &mut env,
            NotificationGroupId::new(1),
            NotificationGroupType::Messages,
            DialogId::new(1),
            999,
            DialogId::new(1),
            false,
            -1,
            0,
            NotificationId::new(1),
            NotificationType::new_message(MessageId::new(1 << 20), true),
            "test",
        );

        let group_key = manager.get_group(NotificationGroupId::new(1)).unwrap();
        assert_eq!(group_key.dialog_id, DialogId::new(1));
        assert_eq!(group_key.last_notification_date, 0);
        let group = manager.groups.get(&group_key).unwrap();
        assert_eq!(group.r#type, NotificationGroupType::Messages);
        assert_eq!(group.pending_notifications.len(), 1);
        assert_eq!(manager.delayed_notification_update_count, 1);
        // delay = max(0, 0) - passed(0) → clamped to the minimum → 1 ms
        assert_eq!(
            manager.flush_pending_notifications_timeout.get(&1),
            Some(&10.001)
        );

        // duplicate id → rejected
        manager.add_notification(
            &mut env,
            NotificationGroupId::new(1),
            NotificationGroupType::Messages,
            DialogId::new(1),
            999,
            DialogId::new(1),
            false,
            -1,
            0,
            NotificationId::new(1),
            NotificationType::new_message(MessageId::new(2 << 20), true),
            "test",
        );
        assert_eq!(
            manager
                .groups
                .get(&group_key)
                .unwrap()
                .pending_notifications
                .len(),
            1
        );

        // duplicate object id → rejected
        manager.add_notification(
            &mut env,
            NotificationGroupId::new(1),
            NotificationGroupType::Messages,
            DialogId::new(1),
            999,
            DialogId::new(1),
            false,
            -1,
            0,
            NotificationId::new(2),
            NotificationType::new_message(MessageId::new(1 << 20), true),
            "test",
        );
        assert_eq!(
            manager
                .groups
                .get(&group_key)
                .unwrap()
                .pending_notifications
                .len(),
            1
        );

        // a later notification in order is accepted
        manager.add_notification(
            &mut env,
            NotificationGroupId::new(1),
            NotificationGroupType::Messages,
            DialogId::new(1),
            999,
            DialogId::new(1),
            false,
            -1,
            0,
            NotificationId::new(2),
            NotificationType::new_message(MessageId::new(2 << 20), true),
            "test",
        );
        assert_eq!(
            manager
                .groups
                .get(&group_key)
                .unwrap()
                .pending_notifications
                .len(),
            2
        );

        // a different settings dialog forces its creation
        manager.add_notification(
            &mut env,
            NotificationGroupId::new(1),
            NotificationGroupType::Messages,
            DialogId::new(1),
            999,
            DialogId::new(3),
            false,
            -1,
            0,
            NotificationId::new(3),
            NotificationType::new_message(MessageId::new(3 << 20), true),
            "test",
        );
        assert_eq!(env.force_created_dialogs, vec![DialogId::new(3)]);
        assert_eq!(
            manager
                .groups
                .get(&group_key)
                .unwrap()
                .pending_notifications
                .len(),
            3
        );
    }

    #[test]
    fn add_notification_disabled_drops_the_notification() {
        let mut manager = NotificationManager {
            max_notification_group_count: 5,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();
        env.disabled = true;

        manager.add_notification(
            &mut env,
            NotificationGroupId::new(1),
            NotificationGroupType::Messages,
            DialogId::new(1),
            999,
            DialogId::new(1),
            false,
            -1,
            0,
            NotificationId::new(1),
            NotificationType::new_message(MessageId::new(1 << 20), true),
            "test",
        );
        assert!(manager.groups.is_empty());
    }

    #[test]
    fn load_groups_from_database_pages() {
        let mut manager = NotificationManager {
            max_notification_group_count: 5,
            max_notification_group_size: 10,
            keep_notification_group_size: 20,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        // two groups are available in the (fake) database
        env.db_group_keys = vec![
            NotificationGroupKey::new(NotificationGroupId::new(2), DialogId::new(2), 200),
            NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(1), 100),
        ];
        env.db_groups.insert(
            NotificationGroupId::new(1),
            NotificationGroupFromDatabase {
                dialog_id: DialogId::new(1),
                r#type: NotificationGroupType::Messages,
                total_count: 1,
                notifications: vec![message_notification(1, 100, 1 << 20)],
            },
        );
        env.db_groups.insert(
            NotificationGroupId::new(2),
            NotificationGroupFromDatabase {
                dialog_id: DialogId::new(2),
                r#type: NotificationGroupType::Mentions,
                total_count: 2,
                notifications: vec![
                    message_notification(2, 190, 2 << 20),
                    message_notification(3, 200, 3 << 20),
                ],
            },
        );

        manager.last_loaded_notification_group_key = NotificationGroupKey::default();
        manager
            .last_loaded_notification_group_key
            .last_notification_date = i32::MAX;
        let loaded = manager.load_message_notification_groups_from_database(&mut env, 3, false);
        assert_eq!(loaded, 2);
        assert_eq!(manager.groups.len(), 2);
        assert!(manager.get_group(NotificationGroupId::new(1)).is_some());
        assert!(manager.get_group(NotificationGroupId::new(2)).is_some());
        assert_eq!(manager.current_notification_id, NotificationId::new(3));

        // fewer rows than the limit reset the cursor
        assert_eq!(
            manager
                .last_loaded_notification_group_key
                .last_notification_date,
            0
        );
        let loaded = manager.load_message_notification_groups_from_database(&mut env, 3, false);
        assert_eq!(loaded, 0);
    }

    #[test]
    fn load_notifications_from_database_extends_the_group() {
        let mut manager = NotificationManager {
            max_notification_group_count: 5,
            max_notification_group_size: 2,
            keep_notification_group_size: 20,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        let group_key =
            NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(1), 100);
        let mut group = NotificationGroup {
            r#type: NotificationGroupType::Messages,
            total_count: 5,
            ..NotificationGroup::default()
        };
        group
            .notifications
            .push(message_notification(4, 100, 4 << 20));
        manager.add_group(group_key, group, "test");

        env.db_notifications = vec![
            message_notification(1, 70, 1 << 20),
            message_notification(2, 80, 2 << 20),
            message_notification(3, 90, 3 << 20),
        ];

        manager.load_notifications_from_database(&mut env, group_key, 4);

        let group_key = manager.get_group(NotificationGroupId::new(1)).unwrap();
        let group = manager.groups.get(&group_key).unwrap();
        assert!(!group.is_being_loaded_from_database);
        // the C++ marks the group fully loaded only when a load returns
        // nothing (or errors); a partial window stays marked not-loaded
        assert!(!group.is_loaded_from_database);
        assert_eq!(group.notifications.len(), 4);
        assert_eq!(
            group.notifications[0].notification_id,
            NotificationId::new(1)
        );
        assert_eq!(
            group.notifications[3].notification_id,
            NotificationId::new(4)
        );
        // the group position moved to the newest loaded date (100 stays —
        // the existing notification is the newest)
        assert_eq!(group_key.last_notification_date, 100);
    }

    #[test]
    fn load_notifications_from_database_error_marks_loaded() {
        let mut manager = NotificationManager {
            max_notification_group_count: 5,
            max_notification_group_size: 2,
            keep_notification_group_size: 20,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();
        env.db_error = true;

        let group_key =
            NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(1), 100);
        let mut group = NotificationGroup {
            r#type: NotificationGroupType::Messages,
            total_count: 5,
            ..NotificationGroup::default()
        };
        group
            .notifications
            .push(message_notification(4, 100, 4 << 20));
        manager.add_group(group_key, group, "test");

        manager.load_notifications_from_database(&mut env, group_key, 4);

        let group_key = manager.get_group(NotificationGroupId::new(1)).unwrap();
        let group = manager.groups.get(&group_key).unwrap();
        assert!(!group.is_being_loaded_from_database);
        assert!(group.is_loaded_from_database);
        assert_eq!(group.notifications.len(), 1);
    }

    #[test]
    fn remove_temporary_notifications_trims_the_group() {
        let mut manager = NotificationManager {
            max_notification_group_count: 5,
            max_notification_group_size: 2,
            keep_notification_group_size: 20,
            is_inited: true,
            is_binlog_processed: true,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        let group_key =
            NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(1), 100);
        let mut group = NotificationGroup {
            r#type: NotificationGroupType::Messages,
            total_count: 3,
            ..NotificationGroup::default()
        };
        group
            .notifications
            .push(message_notification(1, 80, 1 << 20));
        group.notifications.push(Notification::new(
            NotificationId::new(2),
            90,
            false,
            NotificationType::new_push_message(
                UserId::new(1),
                DialogId::default(),
                "sender".to_string(),
                false,
                MessageId::new(2 << 20),
                "MESSAGE_TEXT".to_string(),
                "text".to_string(),
                crate::photo::Photo::default(),
                crate::document::Document::default(),
            ),
        ));
        group.notifications.push(Notification::new(
            NotificationId::new(3),
            100,
            false,
            NotificationType::new_push_message(
                UserId::new(1),
                DialogId::default(),
                "sender".to_string(),
                false,
                MessageId::new(3 << 20),
                "MESSAGE_TEXT".to_string(),
                "text".to_string(),
                crate::photo::Photo::default(),
                crate::document::Document::default(),
            ),
        ));
        manager.add_group(group_key, group, "test");

        manager.remove_temporary_notifications(&mut env, NotificationGroupId::new(1), "test");
        // the removal update is queued, not force-flushed
        manager.force_flush_pending_updates(&mut env, NotificationGroupId::new(1), "test");

        let group_key = manager.get_group(NotificationGroupId::new(1)).unwrap();
        let group = manager.groups.get(&group_key).unwrap();
        assert_eq!(group.notifications.len(), 1);
        assert_eq!(
            group.notifications[0].notification_id,
            NotificationId::new(1)
        );
        assert_eq!(group.total_count, 1);
        // the two temporary notifications were removed via an update
        let group_updates = sent_group_updates(&env.updates);
        assert_eq!(group_updates.len(), 1);
        assert_eq!(group_updates[0].removed_notification_ids, vec![2, 3]);
    }

    fn push_notification(id: i32, date: i32, message_id: i64) -> Notification {
        Notification::new(
            NotificationId::new(id),
            date,
            false,
            NotificationType::new_push_message(
                UserId::new(1),
                DialogId::default(),
                "sender".to_string(),
                false,
                MessageId::new(message_id),
                "MESSAGE_TEXT".to_string(),
                "text".to_string(),
                crate::photo::Photo::default(),
                crate::document::Document::default(),
            ),
        )
    }

    fn push_type(message_id: i64, sender_name: &str) -> NotificationType {
        NotificationType::new_push_message(
            UserId::new(1),
            DialogId::default(),
            sender_name.to_string(),
            false,
            MessageId::new(message_id),
            "MESSAGE_TEXT".to_string(),
            "text".to_string(),
            crate::photo::Photo::default(),
            crate::document::Document::default(),
        )
    }

    fn notifications_of(updates: &[td_api::Update]) -> Vec<&td_api::updateNotification> {
        updates
            .iter()
            .filter_map(|update| match update {
                td_api::Update::updateNotification(update) => Some(update),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn edit_notification_updates_visible_and_pending() {
        let mut manager = NotificationManager {
            max_notification_group_count: 5,
            max_notification_group_size: 2,
            keep_notification_group_size: 20,
            is_inited: true,
            is_binlog_processed: true,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        let group_key =
            NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(1), 90);
        let mut group = NotificationGroup {
            r#type: NotificationGroupType::Messages,
            total_count: 2,
            ..NotificationGroup::default()
        };
        group.notifications.push(push_notification(1, 80, 1 << 20));
        group.notifications.push(push_notification(2, 90, 2 << 20));
        manager.add_group(group_key, group, "test");

        // a visible notification is edited through an updateNotification
        manager.edit_notification(
            &mut env,
            NotificationGroupId::new(1),
            NotificationId::new(1),
            push_type(1 << 20, "edited"),
        );
        manager.force_flush_pending_updates(&mut env, NotificationGroupId::new(1), "test");
        let notification_updates = notifications_of(&env.updates);
        assert_eq!(notification_updates.len(), 1);
        assert_eq!(notification_updates[0].notification.as_ref().unwrap().id, 1);

        // a pending notification is edited in place, without an update
        manager.add_notification(
            &mut env,
            NotificationGroupId::new(1),
            NotificationGroupType::Messages,
            DialogId::new(1),
            100,
            DialogId::new(1),
            false,
            0,
            0,
            NotificationId::new(5),
            push_type(5 << 20, "original"),
            "test",
        );
        manager.edit_notification(
            &mut env,
            NotificationGroupId::new(1),
            NotificationId::new(5),
            push_type(5 << 20, "edited"),
        );
        let group_key = manager.get_group(NotificationGroupId::new(1)).unwrap();
        let group = manager.groups.get(&group_key).unwrap();
        assert!(matches!(
            &group.pending_notifications[0].r#type,
            NotificationType::NewPushMessage { sender_name, .. } if sender_name == "edited"
        ));
        let notification_updates = notifications_of(&env.updates);
        assert_eq!(notification_updates.len(), 1);
    }

    #[test]
    fn edit_notification_ignores_a_type_change() {
        let mut manager = NotificationManager {
            max_notification_group_count: 5,
            max_notification_group_size: 2,
            keep_notification_group_size: 20,
            is_inited: true,
            is_binlog_processed: true,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        let group_key =
            NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(1), 90);
        let mut group = NotificationGroup {
            r#type: NotificationGroupType::Messages,
            total_count: 2,
            ..NotificationGroup::default()
        };
        group.notifications.push(push_notification(1, 80, 1 << 20));
        group.notifications.push(push_notification(2, 90, 2 << 20));
        manager.add_group(group_key, group, "test");

        // changing the object id (message) of a notification is rejected
        manager.edit_notification(
            &mut env,
            NotificationGroupId::new(1),
            NotificationId::new(1),
            push_type(9 << 20, "edited"),
        );
        let group_key = manager.get_group(NotificationGroupId::new(1)).unwrap();
        let group = manager.groups.get(&group_key).unwrap();
        assert!(matches!(
            &group.notifications[0].r#type,
            NotificationType::NewPushMessage { sender_name, .. } if sender_name == "sender"
        ));
        assert!(notifications_of(&env.updates).is_empty());
    }

    #[test]
    fn remove_notification_rejects_invalid_ids() {
        let mut manager = NotificationManager {
            max_notification_group_count: 5,
            max_notification_group_size: 2,
            keep_notification_group_size: 20,
            is_inited: true,
            is_binlog_processed: true,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        let errors = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        for (group_id, notification_id) in [
            (NotificationGroupId::default(), NotificationId::new(1)),
            (NotificationGroupId::new(1), NotificationId::default()),
        ] {
            let errors = std::sync::Arc::clone(&errors);
            let promise =
                PromiseCreator::lambda_result(move |result: tdutils::status::Result<()>| {
                    errors.lock().unwrap().push(result.is_error());
                });
            manager.remove_notification(
                &mut env,
                group_id,
                notification_id,
                true,
                false,
                promise,
                "test",
            );
        }
        assert_eq!(*errors.lock().unwrap(), vec![true, true]);
    }

    #[test]
    fn remove_notification_permanent_pops_the_visible_window() {
        let mut manager = NotificationManager {
            max_notification_group_count: 5,
            max_notification_group_size: 2,
            keep_notification_group_size: 20,
            is_inited: true,
            is_binlog_processed: true,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        let group_key =
            NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(1), 100);
        let mut group = NotificationGroup {
            r#type: NotificationGroupType::Messages,
            total_count: 3,
            ..NotificationGroup::default()
        };
        group
            .notifications
            .push(message_notification(1, 80, 1 << 20));
        group.notifications.push(push_notification(2, 90, 2 << 20));
        group.notifications.push(push_notification(3, 100, 3 << 20));
        manager.add_group(group_key, group, "test");

        manager.remove_notification(
            &mut env,
            NotificationGroupId::new(1),
            NotificationId::new(3),
            true,
            false,
            Promise::default(),
            "test",
        );

        let group_key = manager.get_group(NotificationGroupId::new(1)).unwrap();
        let group = manager.groups.get(&group_key).unwrap();
        assert_eq!(group.notifications.len(), 2);
        assert_eq!(group.total_count, 2);

        // the removal update is queued (the 50 ms flush delay), not sent
        // immediately
        manager.force_flush_pending_updates(&mut env, NotificationGroupId::new(1), "test");
        let group_updates = sent_group_updates(&env.updates);
        assert_eq!(group_updates.len(), 1);
        assert_eq!(group_updates[0].total_count, 2);
        assert_eq!(group_updates[0].removed_notification_ids, vec![3]);
        assert_eq!(group_updates[0].added_notifications.len(), 1);
        assert_eq!(group_updates[0].added_notifications[0].id, 1);
    }

    #[test]
    fn remove_notification_deletes_pending_without_update() {
        let mut manager = NotificationManager {
            max_notification_group_count: 5,
            max_notification_group_size: 2,
            keep_notification_group_size: 20,
            is_inited: true,
            is_binlog_processed: true,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        let group_key = NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(1), 0);
        let group = NotificationGroup {
            r#type: NotificationGroupType::Messages,
            total_count: 1,
            ..NotificationGroup::default()
        };
        manager.add_group(group_key, group, "test");

        manager.add_notification(
            &mut env,
            NotificationGroupId::new(1),
            NotificationGroupType::Messages,
            DialogId::new(1),
            100,
            DialogId::new(1),
            false,
            0,
            0,
            NotificationId::new(5),
            push_type(5 << 20, "sender"),
            "test",
        );
        let group_key = manager.get_group(NotificationGroupId::new(1)).unwrap();
        assert_eq!(
            manager
                .groups
                .get(&group_key)
                .unwrap()
                .pending_notifications
                .len(),
            1
        );

        manager.remove_notification(
            &mut env,
            NotificationGroupId::new(1),
            NotificationId::new(5),
            true,
            false,
            Promise::default(),
            "test",
        );

        let group_key = manager.get_group(NotificationGroupId::new(1)).unwrap();
        let group = manager.groups.get(&group_key).unwrap();
        assert!(group.pending_notifications.is_empty());
        assert_eq!(group.total_count, 1);
        assert!(sent_group_updates(&env.updates).is_empty());
    }

    #[test]
    fn remove_notification_group_deletes_the_prefix() {
        let mut manager = NotificationManager {
            max_notification_group_count: 5,
            max_notification_group_size: 2,
            keep_notification_group_size: 20,
            is_inited: true,
            is_binlog_processed: true,
            ..NotificationManager::default()
        };
        // the C++ clamps max_notification_id to current_notification_id_
        manager.current_notification_id = NotificationId::new(10);
        let mut env = TestEnv::new();

        let group_key =
            NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(1), 100);
        let mut group = NotificationGroup {
            r#type: NotificationGroupType::Messages,
            total_count: 3,
            ..NotificationGroup::default()
        };
        group
            .notifications
            .push(message_notification(1, 80, 1 << 20));
        group.notifications.push(push_notification(2, 90, 2 << 20));
        group.notifications.push(push_notification(3, 100, 3 << 20));
        manager.add_group(group_key, group, "test");

        manager.remove_notification_group(
            &mut env,
            NotificationGroupId::new(1),
            NotificationId::new(2),
            NotificationObjectId::default(),
            -1,
            false,
            Promise::default(),
        );

        let group_key = manager.get_group(NotificationGroupId::new(1)).unwrap();
        let group = manager.groups.get(&group_key).unwrap();
        assert_eq!(group.notifications.len(), 1);
        assert_eq!(
            group.notifications[0].notification_id,
            NotificationId::new(3)
        );
        assert_eq!(group.total_count, 3);

        // the removal update is queued (force_update was false)
        manager.force_flush_pending_updates(&mut env, NotificationGroupId::new(1), "test");
        let group_updates = sent_group_updates(&env.updates);
        assert_eq!(group_updates.len(), 1);
        assert_eq!(group_updates[0].removed_notification_ids, vec![2]);
        assert!(group_updates[0].added_notifications.is_empty());
        assert_eq!(group_updates[0].total_count, 3);
    }

    #[test]
    fn set_notification_total_count_emits_the_count_update() {
        let mut manager = NotificationManager {
            max_notification_group_count: 5,
            max_notification_group_size: 2,
            keep_notification_group_size: 20,
            is_inited: true,
            is_binlog_processed: true,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        let group_key =
            NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(1), 90);
        let mut group = NotificationGroup {
            r#type: NotificationGroupType::Messages,
            total_count: 2,
            ..NotificationGroup::default()
        };
        // message notifications are not temporary — the C++ adds the
        // temporary count to the new total, which would offset it here
        group
            .notifications
            .push(message_notification(1, 80, 1 << 20));
        group
            .notifications
            .push(message_notification(2, 90, 2 << 20));
        manager.add_group(group_key, group, "test");

        manager.set_notification_total_count(&mut env, NotificationGroupId::new(1), 5);

        let group_key = manager.get_group(NotificationGroupId::new(1)).unwrap();
        assert_eq!(manager.groups.get(&group_key).unwrap().total_count, 5);

        // the count update is queued, not sent immediately
        manager.force_flush_pending_updates(&mut env, NotificationGroupId::new(1), "test");
        let group_updates = sent_group_updates(&env.updates);
        assert_eq!(group_updates.len(), 1);
        assert_eq!(group_updates[0].total_count, 5);
        assert!(group_updates[0].added_notifications.is_empty());
        assert!(group_updates[0].removed_notification_ids.is_empty());
    }

    #[test]
    fn get_notification_group_message_ids_returns_visible_ids() {
        let mut manager = NotificationManager {
            max_notification_group_count: 5,
            max_notification_group_size: 2,
            keep_notification_group_size: 20,
            is_inited: true,
            is_binlog_processed: true,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        let group_key =
            NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(1), 90);
        let mut group = NotificationGroup {
            r#type: NotificationGroupType::Messages,
            total_count: 2,
            ..NotificationGroup::default()
        };
        group
            .notifications
            .push(message_notification(1, 80, 1 << 20));
        group.notifications.push(push_notification(2, 90, 2 << 20));
        manager.add_group(group_key, group, "test");
        manager.add_notification(
            &mut env,
            NotificationGroupId::new(1),
            NotificationGroupType::Messages,
            DialogId::new(1),
            100,
            DialogId::new(1),
            false,
            0,
            0,
            NotificationId::new(3),
            push_type(3 << 20, "sender"),
            "test",
        );

        let message_ids =
            manager.get_notification_group_message_ids(&mut env, NotificationGroupId::new(1));
        assert_eq!(
            message_ids,
            vec![
                MessageId::new(1 << 20),
                MessageId::new(2 << 20),
                MessageId::new(3 << 20)
            ]
        );
    }

    #[test]
    fn call_notification_group_ids_are_allocated_and_reused() {
        let mut manager = NotificationManager {
            max_notification_group_count: 5,
            max_notification_group_size: 2,
            keep_notification_group_size: 20,
            is_inited: true,
            is_binlog_processed: true,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        let group_id1 = manager.get_call_notification_group_id(&mut env, DialogId::new(1));
        let group_id2 = manager.get_call_notification_group_id(&mut env, DialogId::new(2));
        assert!(group_id1.is_valid());
        assert!(group_id2.is_valid());
        assert_ne!(group_id1, group_id2);
        assert_eq!(
            env.pmc[NOTIFICATION_CALL_GROUP_IDS_KEY],
            format!("{},{}", group_id1.get(), group_id2.get())
        );
        assert_eq!(
            manager.get_call_notification_group_id(&mut env, DialogId::new(1)),
            group_id1
        );

        manager.add_call_notification(&mut env, DialogId::new(1), CallId::new(7));
        assert_eq!(
            manager.active_call_notifications[&DialogId::new(1)].len(),
            1
        );

        manager.remove_call_notification(&mut env, DialogId::new(1), CallId::new(7));
        assert!(
            !manager
                .active_call_notifications
                .contains_key(&DialogId::new(1))
        );
        assert!(manager.get_group(group_id1).is_none());
        assert!(
            manager
                .available_call_notification_group_ids
                .contains(&group_id1)
        );

        // the only available id is reused for a new dialog
        assert_eq!(
            manager.get_call_notification_group_id(&mut env, DialogId::new(3)),
            group_id1
        );
    }

    #[test]
    fn remove_temporary_notification_by_object_id_removes_both_planes() {
        let mut manager = NotificationManager {
            max_notification_group_count: 5,
            max_notification_group_size: 2,
            keep_notification_group_size: 20,
            is_inited: true,
            is_binlog_processed: true,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        let group_key =
            NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(1), 90);
        let mut group = NotificationGroup {
            r#type: NotificationGroupType::Messages,
            total_count: 2,
            ..NotificationGroup::default()
        };
        group
            .notifications
            .push(message_notification(1, 80, 1 << 20));
        group.notifications.push(push_notification(2, 90, 2 << 20));
        manager.add_group(group_key, group, "test");
        manager.add_notification(
            &mut env,
            NotificationGroupId::new(1),
            NotificationGroupType::Messages,
            DialogId::new(1),
            100,
            DialogId::new(1),
            false,
            0,
            0,
            NotificationId::new(3),
            push_type(3 << 20, "sender"),
            "test",
        );

        // a pending and a stored notification share the object id 2 << 20
        manager.add_notification(
            &mut env,
            NotificationGroupId::new(1),
            NotificationGroupType::Messages,
            DialogId::new(1),
            100,
            DialogId::new(1),
            false,
            0,
            0,
            NotificationId::new(4),
            push_type(2 << 20, "sender"),
            "test",
        );

        manager.remove_temporary_notification_by_object_id(
            &mut env,
            NotificationGroupId::new(1),
            NotificationObjectId::from_message_id(MessageId::new(2 << 20)),
            false,
            "test",
        );

        let group_key = manager.get_group(NotificationGroupId::new(1)).unwrap();
        let group = manager.groups.get(&group_key).unwrap();
        assert_eq!(group.notifications.len(), 1);
        assert_eq!(
            group.notifications[0].notification_id,
            NotificationId::new(1)
        );
        assert!(
            group
                .pending_notifications
                .iter()
                .all(|pending_notification| pending_notification.notification_id
                    != NotificationId::new(4))
        );
    }

    #[test]
    fn tick_timeouts_flushes_pending_notifications() {
        let mut manager = NotificationManager {
            max_notification_group_count: 5,
            max_notification_group_size: 2,
            keep_notification_group_size: 20,
            is_inited: true,
            is_binlog_processed: true,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        let group_key = NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(1), 0);
        let group = NotificationGroup {
            r#type: NotificationGroupType::Messages,
            total_count: 0,
            ..NotificationGroup::default()
        };
        manager.add_group(group_key, group, "test");

        manager.add_notification(
            &mut env,
            NotificationGroupId::new(1),
            NotificationGroupType::Messages,
            DialogId::new(1),
            100,
            DialogId::new(1),
            false,
            0,
            0,
            NotificationId::new(1),
            push_type(1 << 20, "sender"),
            "test",
        );
        assert!(!manager.flush_pending_notifications_timeout.is_empty());

        // the deadline has not passed yet
        manager.tick_timeouts(&mut env);
        let group_key = manager.get_group(NotificationGroupId::new(1)).unwrap();
        assert_eq!(
            manager
                .groups
                .get(&group_key)
                .unwrap()
                .pending_notifications
                .len(),
            1
        );

        env.time += 1.0;
        manager.tick_timeouts(&mut env);
        assert!(manager.flush_pending_notifications_timeout.is_empty());

        manager.force_flush_pending_updates(&mut env, NotificationGroupId::new(1), "test");
        let group_updates = sent_group_updates(&env.updates);
        assert_eq!(group_updates.len(), 1);
        assert_eq!(group_updates[0].added_notifications.len(), 1);
        assert_eq!(group_updates[0].added_notifications[0].id, 1);
        let group_key = manager.get_group(NotificationGroupId::new(1)).unwrap();
        assert_eq!(manager.groups.get(&group_key).unwrap().total_count, 1);
    }

    #[test]
    fn after_get_difference_removes_temporary_notifications() {
        let mut manager = NotificationManager {
            max_notification_group_count: 5,
            max_notification_group_size: 2,
            keep_notification_group_size: 20,
            is_inited: true,
            is_binlog_processed: true,
            ..NotificationManager::default()
        };
        let mut env = TestEnv::new();

        let group_key =
            NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(1), 90);
        let mut group = NotificationGroup {
            r#type: NotificationGroupType::Messages,
            total_count: 2,
            ..NotificationGroup::default()
        };
        group
            .notifications
            .push(message_notification(1, 80, 1 << 20));
        group.notifications.push(push_notification(2, 90, 2 << 20));
        manager.add_group(group_key, group, "test");

        manager.before_get_difference(&mut env);
        manager.after_get_difference(&mut env);
        env.time += 1.0;
        manager.tick_timeouts(&mut env);

        manager.force_flush_pending_updates(&mut env, NotificationGroupId::new(1), "test");
        let group_key = manager.get_group(NotificationGroupId::new(1)).unwrap();
        let group = manager.groups.get(&group_key).unwrap();
        assert_eq!(group.notifications.len(), 1);
        assert_eq!(
            group.notifications[0].notification_id,
            NotificationId::new(1)
        );
        assert_eq!(group.total_count, 1);
        let group_updates = sent_group_updates(&env.updates);
        assert_eq!(group_updates.len(), 1);
        assert_eq!(group_updates[0].removed_notification_ids, vec![2]);
    }

    #[test]
    fn process_push_notification_runs_the_local_bands_and_parks() {
        let mut env = TestEnv::new();
        let mut manager = NotificationManager::default();

        // the disabled/"{}" band answers ok inline (the C++ error-200
        // mapping over the wrapping lambda)
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let capture = slot.clone();
        let promise = PromiseCreator::lambda_result(move |r| *capture.lock().unwrap() = Some(r));
        manager.process_push_notification(&mut env, "{}".to_string(), promise);
        assert!(matches!(
            *slot.lock().unwrap(),
            Some(tdutils::status::Result::Ok(()))
        ));
        assert!(manager.parked_process_push_promises.is_empty());

        // an unparseable payload errors with the receiver-id band
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let capture = slot.clone();
        let promise = PromiseCreator::lambda_result(move |r| *capture.lock().unwrap() = Some(r));
        manager.process_push_notification(&mut env, "garbage".to_string(), promise);
        assert!(
            matches!(*slot.lock().unwrap(), Some(tdutils::status::Result::Err(ref status)) if status.code() == 400),
            "expected the receiver-id error"
        );

        // a real payload parks at the decrypt/run seam
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let capture = slot.clone();
        let promise = PromiseCreator::lambda_result(move |r| *capture.lock().unwrap() = Some(r));
        manager.process_push_notification(
            &mut env,
            "{\"p\":\"abcdefghabcdefghabcdefghabcdefgh\"}".to_string(),
            promise,
        );
        assert_eq!(manager.parked_process_push_promises.len(), 1);
        assert!((*slot.lock().unwrap()).is_none());
    }
}
