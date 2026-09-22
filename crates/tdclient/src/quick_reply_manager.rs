//! `QuickReplyManager` (td/telegram/QuickReplyManager.{h,cpp}) — the M.37
//! Wave-3 quick-reply manager: the shortcut-name validation and the
//! shortcut map with the four request flows (setName/delete/reorder/
//! deleteMessages — the only quick-reply td_api arms in this revision).
//! Recorded collapses: the Shortcut/QuickReplyMessage byte-exact
//! store/parse + the object builders ride the M.9/M.10 content and
//! reply-markup surfaces (unmerged); the eight query handlers are
//! `[blocked: net]` env seams with parked promises.

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

    // ---- the query seams ([blocked: net]; the promises park) ----
    fn send_edit_quick_reply_shortcut_query(&mut self, shortcut_id: i32, name: &str) {
        tdutils::log_info!(
            "Send messages.editQuickReplyShortcut({shortcut_id}, {name}) — [blocked: net]"
        );
    }
    fn send_delete_quick_reply_shortcut_query(&mut self, shortcut_id: i32) {
        tdutils::log_info!(
            "Send messages.deleteQuickReplyShortcut({shortcut_id}) — [blocked: net]"
        );
    }
    fn send_reorder_quick_replies_query(&mut self, shortcut_ids: &[i32]) {
        tdutils::log_info!("Send messages.reorderQuickReplies — [blocked: net]");
        let _ = shortcut_ids;
    }
    fn send_delete_quick_reply_messages_query(&mut self, shortcut_id: i32, message_ids: &[i64]) {
        tdutils::log_info!(
            "Send messages.deleteQuickReplyMessages({shortcut_id}) — [blocked: net]"
        );
        let _ = message_ids;
    }
}

/// The collapsed `QuickReplyManager` — the shortcut name → id map and the
/// request flows.
#[derive(Default)]
pub struct QuickReplyManager {
    /// `shortcuts_` — the id → name map (the full Shortcut records join
    /// the store/parse port).
    shortcuts: std::collections::BTreeMap<i32, String>,
    /// The `()`-typed promises parked at their `[blocked: net]` query
    /// sends.
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
        self.net_blocked_promises.push(promise);
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
        self.net_blocked_promises.push(promise);
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
        self.net_blocked_promises.push(promise);
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
        self.net_blocked_promises.push(promise);
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
        assert_eq!(manager.net_blocked_promises.len(), 1);
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
