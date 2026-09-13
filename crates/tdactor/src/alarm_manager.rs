//! Port of `td/telegram/AlarmManager.{h,cpp}` — the "Alarm" step of S35.
//!
//! It is a td-layer actor in C++; it lives here (instead of a `td` crate)
//! until the td/telegram port exists, because it uses nothing beyond the
//! tdactor/tdutils APIs. The move is a file move.
//!
//! # Mapping notes
//!
//! - The C++ constructor wires the member [`MultiTimeout`] to
//!   `on_alarm_timeout_callback` + `this` (the member-actor idiom); sound
//!   Rust resolves the owner's [`ActorId`] inside `start_up` (the first
//!   dispatched event, where the context is valid) and captures it in the
//!   callback, which then does the C++ `send_closure_later` hop back into
//!   `on_alarm_timeout`.
//! - C++ `tear_down` fails every pending promise with
//!   `Global::request_aborted_error()` (`Status::Error(500, "Request
//!   aborted")`); the local `request_aborted_error` stands in for the
//!   `Global` accessor. The member `MultiTimeout` is dropped right after
//!   `tear_down`, which cancels every remaining timeout — the per-key
//!   `alarm_timeout_.cancel_timeout` calls in the C++ loop become implicit.
//! - `parent_.reset()` → the [`ActorShared`] is replaced with an empty one;
//!   the replacement drops the non-empty value, which sends the `hangup` to
//!   the parent (the port's `ActorShared` hangup-on-drop semantics).

use std::collections::HashMap;

use tdutils::promise::Promise;
use tdutils::status::Status;

use crate::actor::Actor;
use crate::actor_id::{ActorId, ActorRef, ActorShared};
use crate::scheduler::Scheduler;
use crate::timeout::MultiTimeout;

/// `Global::request_aborted_error()`.
fn request_aborted_error() -> Status {
    Status::error(500, "Request aborted")
}

/// The port of C++ `class AlarmManager`: hands out one-shot alarms, each
/// resolving its own [`Promise<()>`](Promise) when it expires.
pub struct AlarmManager {
    parent: ActorShared<()>,
    /// C++ `int64 alarm_id_ = 1`.
    alarm_id: i64,
    /// C++ `FlatHashMap<int64, Promise<Unit>> pending_alarms_`.
    pending_alarms: HashMap<i64, Promise<()>>,
    /// C++ `MultiTimeout alarm_timeout_{"AlarmTimeout"}` — the member actor,
    /// registered by pointer at construction (see the `timeout` module docs).
    alarm_timeout: Box<MultiTimeout>,
}

impl AlarmManager {
    /// The C++ constructor: creates the member [`MultiTimeout`] (which
    /// registers on the current scheduler), so a scheduler context is
    /// required, as in C++.
    pub fn new(parent: ActorShared<()>) -> Box<Self> {
        Box::new(Self {
            parent,
            alarm_id: 1,
            pending_alarms: HashMap::new(),
            alarm_timeout: MultiTimeout::new("AlarmTimeout"),
        })
    }

    /// The port of C++ `AlarmManager::set_alarm`.
    ///
    /// The callback was set in [`start_up`](Actor::start_up); arming a key
    /// before `start_up` is impossible the same way it is in C++ (alarms are
    /// only requested through actor messages, which are dispatched after
    /// `start_up`).
    pub fn set_alarm(&mut self, seconds: f64, mut promise: Promise<()>) {
        if !(0.0..=3e9).contains(&seconds) {
            promise.set_error(Status::error(400, "Wrong parameter seconds specified"));
            return;
        }

        let alarm_id = self.alarm_id;
        self.alarm_id += 1;
        self.pending_alarms.insert(alarm_id, promise);
        self.alarm_timeout.set_timeout_in(alarm_id, seconds);
    }

    /// The port of C++ `AlarmManager::on_alarm_timeout`.
    fn on_alarm_timeout(&mut self, alarm_id: i64) {
        if let Some(mut promise) = self.pending_alarms.remove(&alarm_id) {
            promise.set_value(());
        }
    }

    /// The port of C++ `AlarmManager::tear_down`.
    fn tear_down_impl(&mut self) {
        for (_, mut promise) in self.pending_alarms.drain() {
            promise.set_error(request_aborted_error());
        }
        self.parent = ActorShared::default();
    }
}

impl Actor for AlarmManager {
    fn start_up(&mut self) {
        // C++ (in the constructor): `alarm_timeout_.set_callback(
        // on_alarm_timeout_callback); alarm_timeout_.set_callback_data(this);`
        // — the callback pointer pair becomes a closure capturing this
        // actor's id (the `start_up` context is the first place where the
        // id is available).
        let manager: ActorId<Self> = self.actor_id_of::<Self>();
        self.alarm_timeout.set_callback(Box::new(move |alarm_id| {
            // C++ `AlarmManager::on_alarm_timeout_callback`:
            // `send_closure_later(alarm_manager_id, &AlarmManager::on_alarm_timeout, alarm_id)`.
            Scheduler::instance().send_closure_later(
                ActorRef::from(manager.clone()),
                Box::new(move |actor| {
                    // SAFETY: the `ActorId<AlarmManager>` marker names the
                    // actor's real type — the C++ `static_cast` in
                    // `send_closure` (see the promise module docs).
                    let manager = unsafe { &mut *(actor as *mut dyn Actor as *mut Self) };
                    manager.on_alarm_timeout(alarm_id);
                }),
            );
        }));
    }

    fn tear_down(&mut self) {
        self.tear_down_impl();
    }
}
