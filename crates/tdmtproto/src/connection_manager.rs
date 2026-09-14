//! Port of `td/mtproto/ConnectionManager.{h,cpp}` (S64): tracks the number
//! of active direct and proxy connections. The C++ actor message plumbing
//! (inc/dec via `send_closure`, the `loop()` reaction on the first/last
//! connection) is replaced by atomic counters — the Track H client layer
//! will host the reactions when it drives connection creation.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// `td::mtproto::ConnectionManager`.
#[derive(Debug, Default)]
pub struct ConnectionManager {
    connect_cnt: AtomicU32,
    connect_proxy_cnt: AtomicU32,
}

impl ConnectionManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// `ConnectionManager::connection` — a direct-connection token.
    pub fn connection(self: &Arc<Self>) -> ConnectionToken {
        self.inc(false);
        ConnectionToken {
            manager: Arc::clone(self),
            proxy: false,
        }
    }

    /// `ConnectionManager::connection_proxy` — a proxy-connection token.
    pub fn connection_proxy(self: &Arc<Self>) -> ConnectionToken {
        self.inc(true);
        ConnectionToken {
            manager: Arc::clone(self),
            proxy: true,
        }
    }

    pub fn direct_connections(&self) -> u32 {
        self.connect_cnt.load(Ordering::Acquire)
    }

    pub fn proxy_connections(&self) -> u32 {
        self.connect_proxy_cnt.load(Ordering::Acquire)
    }

    fn inc(&self, proxy: bool) {
        let counter = if proxy {
            &self.connect_proxy_cnt
        } else {
            &self.connect_cnt
        };
        counter.fetch_add(1, Ordering::AcqRel);
    }

    fn dec(&self, proxy: bool) {
        let counter = if proxy {
            &self.connect_proxy_cnt
        } else {
            &self.connect_cnt
        };
        let previous = counter.fetch_sub(1, Ordering::AcqRel);
        assert!(previous > 0, "dec_connect without inc_connect");
    }
}

/// `td::mtproto::ConnectionManager::ConnectionToken` — RAII, decrements on
/// drop.
pub struct ConnectionToken {
    manager: Arc<ConnectionManager>,
    proxy: bool,
}

impl ConnectionToken {
    pub fn empty() -> Self {
        Self {
            manager: Arc::new(ConnectionManager::new()),
            proxy: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        // The C++ token keeps the manager alive until reset; the Rust token
        // always owns one.
        false
    }
}

impl Drop for ConnectionToken {
    fn drop(&mut self) {
        self.manager.dec(self.proxy);
    }
}
