//! The `td/telegram/RequestActor.h` machine (M.11.2), collapsed.
//!
//! C++ runs every non-synchronous request in a `RequestActor<T>` actor: the
//! constructor takes `(ActorShared<Td>, request_id)`, `loop()` checks the
//! close flag, builds a `PromiseActor`/`FutureActor` pair, calls the
//! virtual `do_run(promise)`, and either settles synchronously (the future
//! is ready) or parks the future and decrements `tries_left_` (default 2,
//! per-function `set_tries`). `raw_event` re-enters with the completed
//! future: an error answers `do_send_error`, a result feeds `do_set_result`
//! and `loop()`s again — the retry runs `do_run` a second time (the
//! per-function `get_tries() < N` force escalations, e.g.
//! `GetUserRequest`'s `set_tries(3)`). Exhausted tries answer
//! `500 "Requested data is inaccessible"`; a dropped promise answers
//! `500 "Query can't be answered due to a bug in TDLib"` (or the aborted
//! error while closing), and `hangup`/`loop()` under the close flag answer
//! `Global::request_aborted_error()`.
//!
//! `RequestOnceActor` overrides `loop()`: after any completed pass with
//! `get_tries() < 2` it answers the default `do_send_result()` (`td_api::ok`)
//! and stops — the request's real result rides the fired query promise, the
//! client gets a plain ok as soon as the underlying work completes once.
//!
//! The Rust managers are not actors: their local halves run inline inside
//! `run_request` and their network tails park `Promise`s that re-enter
//! through the deferred-results outbox. The observable machine collapses to
//! these paths (each pinned by the tests in this module and in `td.rs`):
//! - synchronous settle → the immediate `send_result`/`send_error`;
//! - park → later drain → the promise's result/error through the outbox;
//! - park → close → `Td::clear_requests` answers every tracked id with
//!   `401 "Unauthorized"` (destroy) or `500 "Request aborted"`; the
//!   manager-dropped promises re-enter for already-untracked ids and are
//!   dropped, exactly like the C++ promise-actor hangups after `clear`;
//! - the per-function retry ladder lives inside the manager loaders
//!   (`UserManager::load_user(id, tries)`), whose Ok(false) re-parks model
//!   the re-`do_run`; the exhaustion answer is
//!   `TRIES_EXHAUSTED_ERROR`;
//! - `RequestOnceActor` = the CREATE_OK_REQUEST_PROMISE arms: the promise
//!   maps every ok to `td_api::ok` and the parked query completes it.
//!
//! No manager slice edits the dispatcher internals (the M.11 flip protocol,
//! `RUST_REWRITE_PLAN_REQUESTS_M11.md`): a flip lands its arms in
//! `requests.rs`, extends the census fixture, and records corpus coverage.

use crate::td_api::{self, Object};

/// `Global::request_aborted_error()` (Global.h:701-703) — the close-path
/// answer for every in-flight request.
pub const REQUEST_ABORTED_CODE: i32 = 500;
pub const REQUEST_ABORTED_MESSAGE: &str = "Request aborted";

/// `RequestActor::loop` tries exhaustion (RequestActor.h:56-59).
pub const TRIES_EXHAUSTED_CODE: i32 = 500;
pub const TRIES_EXHAUSTED_MESSAGE: &str = "Requested data is inaccessible";

/// `RequestActor::raw_event`'s lost-promise branch (RequestActor.h:74-76) —
/// unreachable in the collapsed form (there are no actor hops to lose); the
/// constant keeps the parity surface documented.
pub const PROMISE_LOST_CODE: i32 = 500;
pub const PROMISE_LOST_MESSAGE: &str = "Query can't be answered due to a bug in TDLib";

/// `RequestOnceActor`'s default result (`RequestActor::do_send_result`,
/// RequestActor.h:120-122).
pub(crate) fn ok_object() -> Object {
    Object::ok(td_api::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_constants_match_the_cpp_strings() {
        // Global.h:701-703.
        assert_eq!(
            (REQUEST_ABORTED_CODE, REQUEST_ABORTED_MESSAGE),
            (500, "Request aborted")
        );
        // RequestActor.h:58.
        assert_eq!(
            (TRIES_EXHAUSTED_CODE, TRIES_EXHAUSTED_MESSAGE),
            (500, "Requested data is inaccessible")
        );
        // RequestActor.h:76.
        assert_eq!(
            (PROMISE_LOST_CODE, PROMISE_LOST_MESSAGE),
            (500, "Query can't be answered due to a bug in TDLib")
        );
        // RequestActor.h:120-122 — the RequestOnce default result.
        assert!(matches!(ok_object(), Object::ok(_)));
    }
}
