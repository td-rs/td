//! Port of the minimal `td/telegram/UpdatesManager.{h,cpp}` (S72): the PTS
//! state machine (the C++ `PtsManager` + `ChangesProcessor<int32>`), the
//! `add_pending_pts_update` validation / PTS-jump recovery / gap trigger,
//! and the update application order (users/chats first, then the message
//! updates with pts accounting).
//!
//! The C++ fan-out goes to UserManager / MessagesManager / DialogManager
//! (all Wave-1, unported) — here it lands in the [`UpdatesSink`] trait,
//! which those managers implement later. The `getDifference` orchestration
//! (gap fill, seq-based updates too long, the pending queue during a
//! difference) is reduced to the [`UpdatesSink::on_gap`] trigger and the
//! `process_pts_update` path; the full recovery loop lands with the
//! session-backed query layer. The "recorded C++ session replay"
//! verification of the plan arrives with the S73 recorder corpus.

use std::collections::BTreeMap;

use tdmtproto::telegram_api;

/// `td::PtsManager` — the db/mem pts pair plus the ordered in-flight
/// changes (`ChangesProcessor<int32>`: a `BTreeMap` keyed by the add id;
/// `finish` folds every finished prefix into `db_pts`).
#[derive(Debug, Default)]
pub struct PtsManager {
    db_pts: i32,
    mem_pts: i32,
    changes: BTreeMap<u64, i32>,
    next_id: u64,
}

impl PtsManager {
    pub fn init(&mut self, pts: i32) {
        self.db_pts = pts;
        self.mem_pts = pts;
        self.changes.clear();
        self.next_id = 0;
    }

    /// `PtsManager::add_pts` — 0 when not a checkpoint.
    pub fn add_pts(&mut self, pts: i32) -> u64 {
        if pts > 0 {
            self.mem_pts = pts;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.changes.insert(id, pts);
        id
    }

    /// `PtsManager::finish` — the finished prefix folds into `db_pts`
    /// (the C++ `state_helper_.finish(pts_id, cb)`).
    pub fn finish(&mut self, pts_id: u64) -> i32 {
        let ids: Vec<u64> = self
            .changes
            .keys()
            .take_while(|id| **id <= pts_id)
            .copied()
            .collect();
        for id in ids {
            let pts = self.changes.remove(&id).expect("the id exists");
            if pts != 0 {
                self.db_pts = pts;
            }
        }
        self.db_pts
    }

    pub fn db_pts(&self) -> i32 {
        self.db_pts
    }

    pub fn mem_pts(&self) -> i32 {
        self.mem_pts
    }
}

/// The application sink — the C++ `user_manager_`/`messages_manager_`/
/// `dialog_manager_` fan-out, as a trait until those managers land.
pub trait UpdatesSink {
    fn on_user(&mut self, user: telegram_api::User);
    fn on_chat(&mut self, chat: telegram_api::Chat);
    fn on_new_message(&mut self, message: telegram_api::Message, pts: i32, pts_count: i32);
    fn on_new_channel_message(&mut self, message: telegram_api::Message, pts: i32, pts_count: i32);
    fn on_delete_messages(&mut self, messages: Vec<i32>, pts: i32, pts_count: i32);
    fn on_message_id(&mut self, id: i32, random_id: i64);
    /// A gap was detected — the C++ schedules `getDifference`.
    fn on_gap(&mut self);
}

/// `td::UpdatesManager` — the minimal subset.
#[derive(Debug)]
pub struct UpdatesManager<S: UpdatesSink> {
    pts_manager: PtsManager,
    qts_manager: PtsManager,
    seq: i32,
    sink: S,
    session_count: i32,
    pending_pts_updates: Vec<telegram_api::Update>,
    accumulated_pts: i32,
    last_pts_jump_warning_time: f64,
    running_get_difference: bool,
    gap_timeout_pending: bool,
}

impl<S: UpdatesSink> UpdatesManager<S> {
    pub fn new(sink: S, session_count: i32) -> Self {
        Self {
            pts_manager: PtsManager::default(),
            qts_manager: PtsManager::default(),
            seq: 0,
            sink,
            session_count,
            pending_pts_updates: Vec::new(),
            accumulated_pts: -1,
            last_pts_jump_warning_time: -1e10,
            running_get_difference: false,
            gap_timeout_pending: false,
        }
    }

    pub fn get_pts(&self) -> i32 {
        self.pts_manager.mem_pts()
    }

    pub fn get_qts(&self) -> i32 {
        self.qts_manager.mem_pts()
    }

    pub fn get_seq(&self) -> i32 {
        self.seq
    }

    pub fn set_pts(&mut self, pts: i32) {
        self.pts_manager.init(pts);
    }

    pub fn set_qts(&mut self, qts: i32) {
        self.qts_manager.init(qts);
    }

    pub fn set_seq(&mut self, seq: i32) {
        self.seq = seq;
    }

    pub fn sink(&mut self) -> &mut S {
        &mut self.sink
    }

    /// The C++ `check_pts_update`: the pts-bearing updates must carry sane
    /// pts/pts_count values (this subset checks the message/deletion
    /// updates).
    fn check_pts_update(update: &telegram_api::Update) -> bool {
        let (pts, pts_count) = match update {
            telegram_api::Update::updateNewMessage(u) => (u.pts, u.pts_count),
            telegram_api::Update::updateNewChannelMessage(u) => (u.pts, u.pts_count),
            telegram_api::Update::updateDeleteMessages(u) => (u.pts, u.pts_count),
            _ => return true, // not a pts update
        };
        pts_count >= 0 && pts > pts_count
    }

    /// `UpdatesManager::add_pending_pts_update` (cpp:3032) — the validation,
    /// the PTS-jump recovery and the gap trigger. Returns whether the
    /// update was applied.
    pub fn add_pending_pts_update(
        &mut self,
        update: telegram_api::Update,
        new_pts: i32,
        pts_count: i32,
        now: f64,
        source: &str,
    ) -> bool {
        if pts_count < 0 || new_pts <= pts_count {
            tdutils::log_error!(
                "Receive update with wrong PTS = {new_pts} or pts_count = {pts_count} from {source}"
            );
            return false;
        }
        if !Self::check_pts_update(&update) {
            tdutils::log_error!("Receive wrong PTS update from {source}");
            return false;
        }

        let old_pts = self.get_pts();
        if new_pts < old_pts - 99 && source != "after_get_difference" {
            let need_restore_pts = new_pts < old_pts - 19999;
            if old_pts == 2_100_000_000
                && new_pts < 1_100_000_000
                && pts_count <= 10000
                && self.session_count > 1
            {
                self.set_pts(1);
                self.set_pts_gap_timeout(0.001);
                return false;
            }
            if now > self.last_pts_jump_warning_time + 1.0
                && need_restore_pts
                && !(old_pts == i32::MAX && self.running_get_difference)
            {
                tdutils::log_error!(
                    "Restore PTS after delete_first_messages from {old_pts} to {new_pts}"
                );
                self.last_pts_jump_warning_time = now;
                self.set_pts(new_pts);
                return false;
            }
            tdutils::log_error!("Skipped update with wrong PTS from {old_pts} to {new_pts}");
            return false;
        }

        // The C++ connect test: the update's range (new_pts - pts_count,
        // new_pts] must attach exactly to the current pts; anything else is
        // a backward overlap (already seen) or a forward gap — both postpone
        // the update and schedule getDifference (cpp:3087-3111).
        if old_pts != new_pts - pts_count {
            self.set_pts_gap_timeout(0.0);
            return false;
        }
        self.apply_pts_update(update);
        self.pts_manager.add_pts(new_pts);
        true
    }

    /// The application of one pts update (the MessagesManager fan-out in
    /// the C++; here the sink).
    fn apply_pts_update(&mut self, update: telegram_api::Update) {
        match update {
            telegram_api::Update::updateNewMessage(u) => {
                self.sink.on_new_message(u.message, u.pts, u.pts_count);
            }
            telegram_api::Update::updateNewChannelMessage(u) => {
                self.sink
                    .on_new_channel_message(u.message, u.pts, u.pts_count);
            }
            telegram_api::Update::updateDeleteMessages(u) => {
                self.sink.on_delete_messages(u.messages, u.pts, u.pts_count);
            }
            telegram_api::Update::updateMessageID(u) => {
                self.sink.on_message_id(u.id, u.random_id);
            }
            other => {
                tdutils::log_error!("Unsupported update in apply_pts_update: {:?}", other);
            }
        }
    }

    /// `UpdatesManager::process_pts_update` (cpp:3013) — must be called
    /// only during a getDifference; the C++ CHECKs translate to debug
    /// asserts.
    pub fn process_pts_update(&mut self, update: telegram_api::Update) {
        debug_assert!(
            self.pending_pts_updates.is_empty(),
            "pending queue during getDifference"
        );
        debug_assert_eq!(
            self.accumulated_pts, -1,
            "accumulated pts during getDifference"
        );
        self.apply_pts_update(update);
    }

    /// `UpdatesManager::process_updates` (cpp:2889-3011) — users and chats
    /// first, then the updates, with the date/seq advancement.
    pub fn process_updates(&mut self, updates: telegram_api::updates, now: f64) {
        for user in updates.users {
            self.sink.on_user(user);
        }
        for chat in updates.chats {
            self.sink.on_chat(chat);
        }
        for update in updates.updates {
            self.process_update(update, now);
        }
        if updates.date > 0 {
            self.seq = updates.date;
        }
    }

    /// `UpdatesManager::process_updates_combined`.
    pub fn process_updates_combined(&mut self, combined: telegram_api::updatesCombined, now: f64) {
        self.process_updates(
            telegram_api::updates {
                updates: combined.updates,
                users: combined.users,
                chats: combined.chats,
                date: combined.date,
                seq: combined.seq,
            },
            now,
        );
    }

    /// One update from an `updates` container.
    fn process_update(&mut self, update: telegram_api::Update, now: f64) {
        match &update {
            // The user/chat data arrives through the container's
            // users/chats vectors (the sink's on_user/on_chat); the update
            // entries themselves only invalidate manager caches in the C++.
            telegram_api::Update::updateUser(_) | telegram_api::Update::updateChat(_) => {}
            _ => {
                let (pts, pts_count) = update_pts(&update);
                if pts > 0 {
                    self.add_pending_pts_update(update, pts, pts_count, now, "process_updates");
                } else {
                    self.apply_pts_update(update);
                }
            }
        }
    }

    /// `UpdatesManager::process_update_short` — the `updates.updateShort`
    /// container (a single update with a date).
    pub fn process_update_short(&mut self, short: telegram_api::updateShort, now: f64) {
        self.process_update(short.update, now);
        if short.date > 0 {
            self.seq = short.date;
        }
    }

    /// `UpdatesManager::on_get_pts_update` — a gap-fill update from
    /// getDifference.
    pub fn on_get_pts_update(
        &mut self,
        update: telegram_api::Update,
        new_pts: i32,
        pts_count: i32,
        _promise_marker: (),
    ) {
        if new_pts <= pts_count || pts_count < 0 {
            tdutils::log_error!(
                "Receive update with wrong PTS = {new_pts} or pts_count = {pts_count}"
            );
            return;
        }
        if !Self::check_pts_update(&update) {
            tdutils::log_error!("Receive wrong PTS update");
            return;
        }
        self.pts_manager.add_pts(new_pts);
        self.apply_pts_update(update);
    }

    /// `UpdatesManager::set_pts_gap_timeout` — the C++ timeout actor
    /// scheduling `getDifference`; the port triggers the sink's gap
    /// callback once per pending timeout.
    pub fn set_pts_gap_timeout(&mut self, _delay: f64) {
        if !self.gap_timeout_pending {
            self.gap_timeout_pending = true;
            self.sink.on_gap();
        }
    }

    /// Marks the gap as handled (the getDifference completion path).
    pub fn on_gap_handled(&mut self) {
        self.gap_timeout_pending = false;
    }

    /// The C++ `running_get_difference_` flag for the PTS-jump recovery.
    pub fn set_running_get_difference(&mut self, running: bool) {
        self.running_get_difference = running;
    }
}

/// The message id of any `Message` umbrella variant (test helper).
#[cfg(test)]
fn message_id(message: &telegram_api::Message) -> i32 {
    use telegram_api::Message as M;
    match message {
        M::messageEmpty(_) => 0,
        M::message(m) => m.id,
        M::messageService(m) => m.id,
    }
}

/// The pts/pts_count of a pts-bearing update.
fn update_pts(update: &telegram_api::Update) -> (i32, i32) {
    match update {
        telegram_api::Update::updateNewMessage(u) => (u.pts, u.pts_count),
        telegram_api::Update::updateNewChannelMessage(u) => (u.pts, u.pts_count),
        telegram_api::Update::updateDeleteMessages(u) => (u.pts, u.pts_count),
        _ => (0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct RecordingSink {
        users: Vec<i64>,
        chats: Vec<i64>,
        messages: Vec<(i32, i32, i32)>, // (message_id, pts, pts_count)
        channel_messages: Vec<(i32, i32, i32)>,
        deletions: Vec<(Vec<i32>, i32, i32)>,
        gaps: usize,
    }

    impl UpdatesSink for RecordingSink {
        fn on_user(&mut self, user: telegram_api::User) {
            self.users.push(match user {
                telegram_api::User::user(u) => u.id,
                telegram_api::User::userEmpty(_) => 0,
            });
        }
        fn on_chat(&mut self, chat: telegram_api::Chat) {
            self.chats.push(match chat {
                telegram_api::Chat::chat(c) => c.id,
                telegram_api::Chat::chatEmpty(_) => 0,
                telegram_api::Chat::chatForbidden(_) => 0,
                telegram_api::Chat::channel(c) => c.id,
                _ => 0,
            });
        }
        fn on_new_message(&mut self, message: telegram_api::Message, pts: i32, pts_count: i32) {
            self.messages.push((message_id(&message), pts, pts_count));
        }
        fn on_new_channel_message(
            &mut self,
            message: telegram_api::Message,
            pts: i32,
            pts_count: i32,
        ) {
            self.channel_messages
                .push((message_id(&message), pts, pts_count));
        }
        fn on_delete_messages(&mut self, messages: Vec<i32>, pts: i32, pts_count: i32) {
            self.deletions.push((messages, pts, pts_count));
        }
        fn on_message_id(&mut self, _id: i32, _random_id: i64) {}
        fn on_gap(&mut self) {
            self.gaps += 1;
        }
    }

    fn user(id: i64) -> telegram_api::User {
        // The telegram_api codegen keeps the scheme's case-preserved names
        // (`user`), unlike the CamelCase td_api model.
        telegram_api::User::user(telegram_api::user {
            id,
            ..telegram_api::user::default()
        })
    }

    fn chat(id: i64) -> telegram_api::Chat {
        telegram_api::Chat::chat(telegram_api::chat {
            id,
            ..telegram_api::chat::default()
        })
    }

    fn new_message(id: i32, pts: i32, pts_count: i32) -> telegram_api::Update {
        telegram_api::Update::updateNewMessage(telegram_api::updateNewMessage {
            message: telegram_api::Message::message(telegram_api::message {
                id,
                ..telegram_api::message::default()
            }),
            pts,
            pts_count,
        })
    }

    #[test]
    fn pts_manager_tracks_mem_and_db() {
        let mut manager = PtsManager::default();
        manager.init(10);
        assert_eq!(manager.mem_pts(), 10);
        assert_eq!(manager.db_pts(), 10);
        let id1 = manager.add_pts(12);
        let id2 = manager.add_pts(15);
        assert_eq!(manager.mem_pts(), 15);
        assert_eq!(manager.db_pts(), 10);
        assert_eq!(manager.finish(id1), 12);
        assert_eq!(manager.finish(id2), 15);
        // A zero pts (not a checkpoint) leaves db_pts alone.
        let id3 = manager.add_pts(0);
        manager.add_pts(20);
        assert_eq!(manager.finish(id3), 15);
    }

    #[test]
    fn process_updates_applies_users_chats_then_messages() {
        let sink = RecordingSink::default();
        let mut manager = UpdatesManager::new(sink, 1);
        manager.set_pts(100);
        let updates = telegram_api::updates {
            updates: vec![
                telegram_api::Update::updateUser(telegram_api::updateUser { user_id: 5 }),
                new_message(42, 101, 1),
                telegram_api::Update::updateChat(telegram_api::updateChat { chat_id: 7 }),
                new_message(43, 102, 1),
            ],
            users: vec![user(5)],
            chats: vec![chat(7)],
            date: 12345,
            seq: 0,
        };
        manager.process_updates(updates, 0.0);
        let sink = manager.sink();
        assert_eq!(sink.users, [5]);
        assert_eq!(sink.chats, [7]);
        assert_eq!(sink.messages, [(42, 101, 1), (43, 102, 1)]);
        assert_eq!(sink.gaps, 0, "sequential pts: no gap");
        assert_eq!(manager.get_seq(), 12345);
    }

    #[test]
    fn gap_detection_triggers_the_sink() {
        let sink = RecordingSink::default();
        let mut manager = UpdatesManager::new(sink, 1);
        manager.set_pts(100);
        let applied = manager.add_pending_pts_update(new_message(42, 105, 1), 105, 1, 0.0, "test");
        assert!(!applied);
        assert_eq!(manager.sink().gaps, 1);
        manager.on_gap_handled();
        manager.set_running_get_difference(true);
        manager.set_pts(105);
        manager.set_running_get_difference(false);
        let applied = manager.add_pending_pts_update(new_message(43, 106, 1), 106, 1, 0.0, "test");
        assert!(applied, "the gap was filled");
        assert_eq!(manager.sink().messages, [(43, 106, 1)]);
    }

    #[test]
    fn wrong_pts_updates_are_dropped() {
        let sink = RecordingSink::default();
        let mut manager = UpdatesManager::new(sink, 1);
        manager.set_pts(100);
        assert!(!manager.add_pending_pts_update(new_message(1, 99, 100), 99, 100, 0.0, "test"));
        assert!(!manager.add_pending_pts_update(new_message(2, -5, 1), -5, 1, 0.0, "test"));
        assert!(manager.sink().messages.is_empty());
    }

    #[test]
    fn pts_jump_restore_matches_cpp() {
        let sink = RecordingSink::default();
        let mut manager = UpdatesManager::new(sink, 2);
        manager.set_pts(2_100_000_000);
        assert!(!manager.add_pending_pts_update(
            new_message(1, 1_050_000_000, 1),
            1_050_000_000,
            1,
            0.0,
            "test"
        ));
        assert_eq!(manager.get_pts(), 1, "restore PTS");
        assert_eq!(manager.sink().gaps, 1, "gap timeout after restore");
    }

    #[test]
    fn process_update_short_applies_the_single_update() {
        let sink = RecordingSink::default();
        let mut manager = UpdatesManager::new(sink, 1);
        manager.set_pts(10);
        manager.process_update_short(
            telegram_api::updateShort {
                update: new_message(7, 11, 1),
                date: 999,
            },
            0.0,
        );
        assert_eq!(manager.sink().messages, [(7, 11, 1)]);
        assert_eq!(manager.get_seq(), 999);
    }

    #[test]
    fn on_get_pts_update_applies_during_difference() {
        let sink = RecordingSink::default();
        let mut manager = UpdatesManager::new(sink, 1);
        manager.set_pts(50);
        manager.on_get_pts_update(new_message(8, 51, 1), 51, 1, ());
        assert_eq!(manager.sink().messages, [(8, 51, 1)]);
    }
}
