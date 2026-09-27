//! wasm32-unknown-emscripten shim for `tdnet::tls`.
//!
//! Browsers terminate TLS inside the WebSocket transport (`wss://`), so the
//! native rustls/tokio-rustls backend is not linked on this target. Only the
//! value types the public API needs exist here; every operation returns an
//! unsupported-platform error at runtime. The real browser transport
//! (MTProto over WebSocket) is the follow-on wasm slice.

use tdutils::status::{Result, Status};

/// `SslCtx::VerifyPeer` — the canonical home (re-exported by [`crate::wget`]
/// for its `WgetParams`). Mirrors the native enum exactly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VerifyPeer {
    /// Verify the server certificate against the store.
    #[default]
    On,
    /// Accept any certificate (`SslCtx::VerifyPeer::Off`).
    Off,
}

/// `td::SslCtx` — on wasm there is no rustls backend to configure; keeping
/// the (unit) type preserves the call-site API.
#[derive(Clone, Debug, Default)]
pub struct SslCtx;

impl SslCtx {
    /// The native constructor validates the root store; wasm has no backend.
    pub fn new(_verify_peer: VerifyPeer) -> Result<Self> {
        Result::Err(unsupported())
    }
}

fn unsupported() -> Status {
    Status::error_message("TLS is handled by the browser WebSocket transport on this platform")
}
