//! `QuickReplyManager` (td/telegram/QuickReplyManager.{h,cpp}) — the M.37
//! Wave-3 quick-reply manager: the shortcut-name validation and the
//! shortcut map with the four request flows (setName/delete/reorder/
//! deleteMessages — the only quick-reply td_api arms in this revision).
//! Recorded collapses: the Shortcut/QuickReplyMessage byte-exact
//! store/parse + the object builders ride the M.9/M.10 content and
//! reply-markup surfaces (unmerged); the four shortcut queries ride the
//! client-side dispatcher under their `QuickReplyNetKind` — the answers
//! drain through `Td::on_quick_reply_net_answer` (the ok values drop,
//! `Promise<Unit>`; the delete-messages error tail's
//! `reload_quick_reply_messages` re-entry rides the messages plane and
//! parks as a logged note). The `messages.getQuickReplies` load machine
//! stays a parked seam (messages plane).

use crate::td_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// `is_shortcut_name_letter` (QuickReplyManager.cpp:884-897).
pub fn is_shortcut_name_letter(code: u32) -> bool {
    use tdutils::unicode::{UnicodeSimpleCategory, get_unicode_simple_category};
    let category = get_unicode_simple_category(code);
    if code == '_' as u32 || code == 0x200c || code == 0xb7 || (0xd80..=0xdff).contains(&code) {
        return true;
    }
    matches!(
        category,
        UnicodeSimpleCategory::DecimalNumber | UnicodeSimpleCategory::Letter
    )
}

/// `check_shortcut_name` (QuickReplyManager.cpp:898-918).
pub fn check_shortcut_name(name: &str) -> Result<(), Status> {
    if !tdutils::utf8::check_utf8(name.as_bytes()) {
        return Err(Status::error(400, "Strings must be encoded in UTF-8"));
    }
    let mut length = 0;
    let mut position = 0;
    let bytes = name.as_bytes();
    while position < bytes.len() {
        let (code, size) = tdutils::utf8::next_utf8_unsafe(&bytes[position..]);
        if !is_shortcut_name_letter(code) {
            return Err(Status::error(400, "A letter is not allowed"));
        }
        position += size;
        length += 1;
    }
    if length == 0 {
        return Err(Status::error(400, "Name must be non-empty"));
    }
    if length > 32 {
        return Err(Status::error(400, "Name is too long"));
    }
    Ok(())
}

/// The `G()`/`td_` surface of the manager.
pub trait QuickReplyManagerEnv {
    fn close_flag(&self) -> bool;

    fn send_update(&mut self, update: td_api::Update);

    // ---- the query sends — the dispatch happens at the env arms ----
    fn send_edit_quick_reply_shortcut_query(&mut self, shortcut_id: i32, name: &str) {
        tdutils::log_info!(
            "Send messages.editQuickReplyShortcut({shortcut_id}, {name}) — [net-inert]"
        );
    }
    fn send_delete_quick_reply_shortcut_query(&mut self, shortcut_id: i32) {
        tdutils::log_info!("Send messages.deleteQuickReplyShortcut({shortcut_id}) — [net-inert]");
    }
    fn send_reorder_quick_replies_query(&mut self, shortcut_ids: &[i32]) {
        tdutils::log_info!("Send messages.reorderQuickReplies — [net-inert]");
        let _ = shortcut_ids;
    }
    fn send_delete_quick_reply_messages_query(&mut self, shortcut_id: i32, message_ids: &[i64]) {
        tdutils::log_info!("Send messages.deleteQuickReplyMessages({shortcut_id}) — [net-inert]");
        let _ = message_ids;
    }
}

/// The quick-reply query kinds riding the dispatcher — the C++
/// ResultHandler classes of QuickReplyManager.cpp:91-240.
#[derive(Clone, Copy, Debug)]
pub(crate) enum QuickReplyNetKind {
    /// `EditQuickReplyShortcutQuery` (cpp:91) — answers Updates (dropped).
    EditShortcut,
    /// `DeleteQuickReplyShortcutQuery` (cpp:117) — answers Bool (dropped).
    DeleteShortcut,
    /// `ReorderQuickRepliesQuery` (cpp:144) — answers Bool (dropped).
    Reorder,
    /// `DeleteQuickReplyMessagesQuery` (cpp:209) — answers Updates
    /// (dropped); the error tail re-enters `reload_quick_reply_messages`
    /// with this shortcut id (cpp:233-237).
    DeleteMessages { shortcut_id: i32 },
}

impl QuickReplyNetKind {
    /// Whether `self` names the same query flow as `other` (the payload
    /// rides only the error-arm reload note).
    fn same_flow(self, other: Self) -> bool {
        std::mem::discriminant(&self) == std::mem::discriminant(&other)
    }
}

/// The collapsed `QuickReplyManager` — the shortcut name → id map and the
/// request flows.
#[derive(Default)]
pub struct QuickReplyManager {
    /// `shortcuts_` — the id → name map (the full Shortcut records join
    /// the store/parse port).
    shortcuts: std::collections::BTreeMap<i32, String>,
    /// The net-riding `(kind, promise)` pairs (the C++ handlers' own
    /// promises; the `getQuickReplies` load machine keeps its separate
    /// park below).
    pub(crate) net_promises: Vec<(QuickReplyNetKind, Promise<()>)>,
    /// The `()`-typed promises parked at the `messages.getQuickReplies`
    /// load seam (the messages plane).
    pub net_blocked_promises: Vec<Promise<()>>,
}

impl QuickReplyManager {
    pub fn new() -> QuickReplyManager {
        QuickReplyManager::default()
    }

    /// `on_update_quick_reply_shortcut` — the update intake (the
    /// updateQuickReplyShortcut wire update); the full record intake joins
    /// the model port.
    pub fn on_update_quick_reply_shortcut(
        &mut self,
        env: &mut impl QuickReplyManagerEnv,
        shortcut_id: i32,
        name: String,
    ) {
        self.shortcuts.insert(shortcut_id, name.clone());
        let _ = env;
    }

    /// `set_quick_reply_shortcut_name` (QuickReplyManager.cpp:1369-1393).
    pub fn set_quick_reply_shortcut_name(
        &mut self,
        env: &mut impl QuickReplyManagerEnv,
        shortcut_id: i32,
        name: &str,
        mut promise: Promise<()>,
    ) {
        if let Err(status) = check_shortcut_name(name) {
            return promise.set_error(status);
        }
        let Some(_current_name) = self.shortcuts.get(&shortcut_id) else {
            return promise.set_error(Status::error(400, "Shortcut not found"));
        };
        self.shortcuts.insert(shortcut_id, name.to_string());
        env.send_edit_quick_reply_shortcut_query(shortcut_id, name);
        self.net_promises
            .push((QuickReplyNetKind::EditShortcut, promise));
    }

    /// `get_quick_reply_shortcuts` (QuickReplyManager.cpp:1126-1137).
    /// The C++ answers ok from the loaded shortcuts state and parks the
    /// promise in `load_queries_` until the messages.getQuickReplies
    /// reload lands; the load/reload machinery is the net seam here, so
    /// the promise parks (the loaded fast path joins the intake port).
    pub fn get_quick_reply_shortcuts(&mut self, promise: Promise<()>) {
        self.net_blocked_promises.push(promise);
        tdutils::log_info!("Send messages.getQuickReplies — [blocked: net]");
    }

    /// `delete_quick_reply_shortcut` (QuickReplyManager.cpp:1412-1438).
    pub fn delete_quick_reply_shortcut(
        &mut self,
        env: &mut impl QuickReplyManagerEnv,
        shortcut_id: i32,
        mut promise: Promise<()>,
    ) {
        if self.shortcuts.remove(&shortcut_id).is_none() {
            return promise.set_error(Status::error(400, "Shortcut not found"));
        }
        env.send_delete_quick_reply_shortcut_query(shortcut_id);
        self.net_promises
            .push((QuickReplyNetKind::DeleteShortcut, promise));
    }

    /// `reorder_quick_reply_shortcuts` (QuickReplyManager.cpp:1447-1492).
    pub fn reorder_quick_reply_shortcuts(
        &mut self,
        env: &mut impl QuickReplyManagerEnv,
        shortcut_ids: &[i32],
        mut promise: Promise<()>,
    ) {
        if shortcut_ids
            .iter()
            .any(|id| !self.shortcuts.contains_key(id))
        {
            return promise.set_error(Status::error(400, "Shortcut not found"));
        }
        env.send_reorder_quick_replies_query(shortcut_ids);
        self.net_promises
            .push((QuickReplyNetKind::Reorder, promise));
    }

    /// `delete_quick_reply_shortcut_messages` (QuickReplyManager.cpp).
    pub fn delete_quick_reply_shortcut_messages(
        &mut self,
        env: &mut impl QuickReplyManagerEnv,
        shortcut_id: i32,
        message_ids: &[i64],
        mut promise: Promise<()>,
    ) {
        if !self.shortcuts.contains_key(&shortcut_id) {
            return promise.set_error(Status::error(400, "Shortcut not found"));
        }
        env.send_delete_quick_reply_messages_query(shortcut_id, message_ids);
        self.net_promises
            .push((QuickReplyNetKind::DeleteMessages { shortcut_id }, promise));
    }

    /// Route one net answer by kind — the C++ `on_result`/`on_error`
    /// halves (QuickReplyManager.cpp:104-113/129-138/159-168/226-240):
    /// the ok value drops and the promise resolves Unit; the
    /// delete-messages error tail would re-enter
    /// `reload_quick_reply_messages(shortcut_id)` (cpp:234), which
    /// rides the messages plane — it parks as a logged note until that
    /// plane lands.
    pub(crate) fn on_net_answer(&mut self, kind: QuickReplyNetKind, result: Result<(), Status>) {
        let Some(index) = self
            .net_promises
            .iter()
            .position(|(pending, _)| pending.same_flow(kind))
        else {
            return;
        };
        let (_, mut promise) = self.net_promises.remove(index);
        match result {
            Ok(()) => promise.set_value(()),
            Err(status) => {
                if let QuickReplyNetKind::DeleteMessages { shortcut_id } = kind {
                    tdutils::log_info!(
                        "Reload quick reply messages {shortcut_id} — [messages plane]"
                    );
                }
                promise.set_error(status);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestEnv {
        queries: Vec<String>,
    }

    impl QuickReplyManagerEnv for TestEnv {
        fn close_flag(&self) -> bool {
            false
        }

        fn send_update(&mut self, _update: td_api::Update) {}

        fn send_edit_quick_reply_shortcut_query(&mut self, shortcut_id: i32, name: &str) {
            self.queries.push(format!("edit {shortcut_id} {name}"));
        }

        fn send_delete_quick_reply_shortcut_query(&mut self, shortcut_id: i32) {
            self.queries.push(format!("delete {shortcut_id}"));
        }

        fn send_reorder_quick_replies_query(&mut self, shortcut_ids: &[i32]) {
            self.queries.push(format!("reorder {:?}", shortcut_ids));
        }

        fn send_delete_quick_reply_messages_query(
            &mut self,
            shortcut_id: i32,
            message_ids: &[i64],
        ) {
            self.queries
                .push(format!("deleteMessages {shortcut_id} {:?}", message_ids));
        }
    }

    #[test]
    fn shortcut_name_validation_matches_the_cpp() {
        assert!(check_shortcut_name("hello_world").is_ok());
        assert!(check_shortcut_name("a".repeat(32).as_str()).is_ok());
        assert!(check_shortcut_name("a".repeat(33).as_str()).is_err());
        assert!(check_shortcut_name("").is_err());
        assert!(check_shortcut_name("has space").is_err());
        assert!(
            check_shortcut_name("sіde").is_ok(),
            "the cyrillic і is a letter"
        );
    }

    #[test]
    fn the_flows_check_the_map_and_dispatch() {
        let mut env = TestEnv {
            queries: Vec::new(),
        };
        let mut manager = QuickReplyManager::new();
        manager.on_update_quick_reply_shortcut(&mut env, 1, "first".to_string());

        let (promise, recorded) = record();
        manager.set_quick_reply_shortcut_name(&mut env, 1, "second", promise);
        assert!(env.queries.iter().any(|q| q == "edit 1 second"));
        assert_eq!(manager.net_promises.len(), 1);
        assert!(!recorded.0.lock().unwrap().is_some(), "the promise parks");

        // unknown shortcut errors
        let (promise, recorded) = record();
        manager.set_quick_reply_shortcut_name(&mut env, 2, "second", promise);
        assert!(recorded.0.lock().unwrap().is_some());

        // invalid name errors before any query
        let count = env.queries.len();
        let (promise, recorded) = record();
        manager.set_quick_reply_shortcut_name(&mut env, 1, "bad name", promise);
        assert!(recorded.0.lock().unwrap().is_some());
        assert_eq!(env.queries.len(), count);

        let (promise, _) = record();
        manager.delete_quick_reply_shortcut(&mut env, 1, promise);
        assert!(env.queries.iter().any(|q| q == "delete 1"));
        assert_eq!(manager.net_promises.len(), 2, "the delete parks too");
        let (promise, recorded) = record();
        manager.delete_quick_reply_shortcut(&mut env, 1, promise);
        assert!(recorded.0.lock().unwrap().is_some(), "already deleted");

        manager.on_update_quick_reply_shortcut(&mut env, 1, "first".to_string());
        manager.on_update_quick_reply_shortcut(&mut env, 2, "second".to_string());
        let (promise, _) = record();
        manager.reorder_quick_reply_shortcuts(&mut env, &[2, 1], promise);
        assert!(env.queries.iter().any(|q| q == "reorder [2, 1]"));
        let (promise, recorded) = record();
        manager.reorder_quick_reply_shortcuts(&mut env, &[2, 3], promise);
        assert!(
            recorded.0.lock().unwrap().is_some(),
            "unknown id in the order"
        );

        let (promise, _) = record();
        manager.delete_quick_reply_shortcut_messages(&mut env, 1, &[10], promise);
        assert!(env.queries.iter().any(|q| q == "deleteMessages 1 [10]"));
    }

    struct Recorded(std::sync::Arc<std::sync::Mutex<Option<std::result::Result<(), Status>>>>);

    struct Recording(std::sync::Arc<std::sync::Mutex<Option<std::result::Result<(), Status>>>>);

    impl tdutils::promise::PromiseInterface<()> for Recording {
        fn set_value(&mut self, _value: ()) {
            *self.0.lock().unwrap() = Some(Ok(()));
        }

        fn set_error(&mut self, error: Status) {
            *self.0.lock().unwrap() = Some(Err(error));
        }
    }

    fn record() -> (Promise<()>, Recorded) {
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        (
            Promise::from_interface(Recording(recorded.clone())),
            Recorded(recorded),
        )
    }
}
