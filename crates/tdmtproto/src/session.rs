//! The async driver for a [`SessionConnection`] (S62): the C++ side runs
//! the session inside an actor with an fd subscription and a timeout
//! (`SessionActor`); the tokio equivalent awaits socket readability and the
//! flush-computed wakeup deadline between synchronous flushes.

use std::time::Duration;

use crate::session_connection::{SessionConnection, SessionConnectionCallback};
use tdutils::status::Status;
use tdutils::time;

/// Drives the session until it closes; `on_closed` is delivered through the
/// callback. Returns the final status (OK on a clean close).
pub async fn run_session(
    mut connection: SessionConnection,
    callback: &mut dyn SessionConnectionCallback,
) -> Status {
    loop {
        let wakeup_at = connection.flush(callback).await;
        if wakeup_at == 0.0 {
            // The connection was closed inside flush (on_closed delivered).
            return Status::ok();
        }
        let deadline = tokio::time::Instant::now()
            + Duration::from_secs_f64((wakeup_at - time::now()).max(0.0));
        tokio::select! {
            _ = connection.raw_connection().stream().readable() => {}
            _ = tokio::time::sleep_until(deadline) => {}
        }
    }
}
