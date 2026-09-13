//! Port of `td/actor/impl/Event.h`: scheduler events.
//!
//! A small structure (the C++ one is 16 bytes) used to send events between
//! actors. There are some predefined types of events:
//!
//! - `NoType` — uninitialized event (also the moved-from state)
//! - `Start` — start actor
//! - `Stop` — stop actor
//! - `Yield` — wake up actor
//! - `Timeout` — some timeout has expired
//! - `Hangup` — hang up called
//! - `Raw` — just pass 8 bytes (the C++ `union Raw` is used for convenience)
//! - `Custom` — run an `FnOnce(&mut dyn Actor)` thunk (the C++
//!   `immediate_closure` subset, landed with S34 for the promise machinery;
//!   the `delayed_closure`/per-actor-message-enum half of the closure
//!   machinery stays with S33)
//!
//! Events also carry a `link_token` (the token of the `ActorShared` they were
//! sent through, if any), which the scheduler exposes to the running actor via
//! `Actor::get_link_token` and uses to pick `hangup` vs `hangup_shared`.

use crate::actor::Actor;

/// A `Custom` event payload: a one-shot thunk the scheduler runs on the
/// destination actor (the C++ `Event::immediate_closure`).
pub(crate) type ActorThunk = Box<dyn FnOnce(&mut dyn Actor) + Send>;

/// The predefined event kinds, in the same order as C++ `Event::Type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    NoType,
    Start,
    Stop,
    Yield,
    Timeout,
    Hangup,
    Raw,
    Custom,
}

/// Raw event payload — the `Event::Raw` union in C++ (`void *`, `uint32` and
/// `uint64` are all representable in one `u64`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Raw(pub u64);

impl Raw {
    /// The `u32` member of the C++ union.
    pub fn u32(self) -> u32 {
        self.0 as u32
    }

    /// The `u64` member of the C++ union.
    pub fn u64(self) -> u64 {
        self.0
    }

    /// The pointer member of the C++ union.
    pub fn ptr<T>(self) -> *mut T {
        self.0 as usize as *mut T
    }
}

impl From<u32> for Raw {
    fn from(value: u32) -> Self {
        Raw(u64::from(value))
    }
}

impl From<u64> for Raw {
    fn from(value: u64) -> Self {
        Raw(value)
    }
}

/// A scheduler event, moved by value exactly like the C++ one.
///
/// Only the scheduler mutates `link_token`; actors build events through the
/// factories and receive payloads through their `Actor` callbacks.
pub struct Event {
    pub(crate) ty: EventType,
    pub(crate) link_token: u64,
    pub(crate) raw: u64,
    pub(crate) custom: Option<ActorThunk>,
}

impl Event {
    /// Start the actor (`Actor::start_up`).
    pub fn start() -> Self {
        Self::of_type(EventType::Start)
    }

    /// Stop the actor (`Actor::tear_down`).
    pub fn stop() -> Self {
        Self::of_type(EventType::Stop)
    }

    /// Wake up the actor (`Actor::wakeup`); `yield` is a Rust keyword.
    pub fn yield_() -> Self {
        Self::of_type(EventType::Yield)
    }

    /// Notify the actor that a timeout has expired (`Actor::timeout_expired`).
    pub fn timeout() -> Self {
        Self::of_type(EventType::Timeout)
    }

    /// Hang up the actor (`Actor::hangup` or `Actor::hangup_shared` depending
    /// on the event's link token).
    pub fn hangup() -> Self {
        Self::of_type(EventType::Hangup)
    }

    /// Pass raw payload bytes (`Actor::raw_event`).
    pub fn raw(raw: Raw) -> Self {
        Self {
            ty: EventType::Raw,
            link_token: 0,
            raw: raw.0,
            custom: None,
        }
    }

    /// Run a closure on the destination actor (`Actor` method called by the
    /// scheduler, inline or through the mailbox) — the C++
    /// `Event::immediate_closure` (the `send_closure` thunk).
    pub fn custom(closure: ActorThunk) -> Self {
        Self {
            ty: EventType::Custom,
            link_token: 0,
            raw: 0,
            custom: Some(closure),
        }
    }

    /// A moved-from, empty event — the C++ `Event()` default.
    pub fn no_type() -> Self {
        Self::of_type(EventType::NoType)
    }

    fn of_type(ty: EventType) -> Self {
        Self {
            ty,
            link_token: 0,
            raw: 0,
            custom: None,
        }
    }
}
