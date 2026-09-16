//! `LinkManager` (td/telegram/LinkManager.cpp) — the M.3.9.1 slice: the
//! autologin-token bookkeeping `process_config` drives
//! (LinkManager.cpp:4000-4003). The link parsing/formatting surface
//! (the 4.8K `check_link`/`parse_internal_link` machinery, Wave 2), the
//! domain-list restore and `update_autologin_domains` (cpp:1899-1910,
//! 4005-4018 — the appConfig path) stay with the LinkManager step.

/// The autologin-token state (LinkManager.h:227-229).
#[derive(Debug)]
pub(crate) struct LinkManager {
    autologin_token: String,
    autologin_update_time: f64,
}

impl LinkManager {
    /// The constructor plus the `start_up` timestamp line
    /// (LinkManager.cpp:1895-1901): the token starts empty and its
    /// update time a year in the past. The domain-list pmc restore in
    /// between lands with the domain-list step.
    pub(crate) fn new() -> Self {
        Self {
            autologin_token: String::new(),
            autologin_update_time: tdutils::time::now() - 365.0 * 86400.0,
        }
    }

    /// `LinkManager::update_autologin_token` (cpp:4000-4003).
    pub(crate) fn update_autologin_token(&mut self, autologin_token: String) {
        self.autologin_update_time = tdutils::time::now();
        self.autologin_token = autologin_token;
    }

    /// The current token (`autologin_token_`) — the autologin url
    /// builders (cpp:4092-4124) read it.
    #[allow(dead_code)] // the Wave 2 url builders land; tests pin it now
    pub(crate) fn autologin_token(&self) -> &str {
        &self.autologin_token
    }

    /// The last update time (`autologin_update_time_`) — `Time::now()`
    /// based, i.e. the process clock like the C++ `Time`.
    #[allow(dead_code)] // the Wave 2 url builders land; tests pin it now
    pub(crate) fn autologin_update_time(&self) -> f64 {
        self.autologin_update_time
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `start_up` line (LinkManager.cpp:1901): the token starts
    /// empty and its update time a year in the past — the autologin url
    /// builder's 10000 s freshness gate (cpp:4092) fails closed until a
    /// config supplies one.
    #[test]
    fn the_token_starts_empty_and_a_year_stale() {
        let link_manager = LinkManager::new();
        assert_eq!(link_manager.autologin_token(), "");
        assert!(link_manager.autologin_update_time() <= tdutils::time::now() - 364.0 * 86400.0);
    }

    /// `update_autologin_token` (cpp:4000-4003): the token stores and
    /// the update time jumps to now.
    #[test]
    fn update_autologin_token_stores_and_stamps() {
        let mut link_manager = LinkManager::new();
        let stale = link_manager.autologin_update_time();
        let before = tdutils::time::now();
        link_manager.update_autologin_token("token".to_string());
        assert_eq!(link_manager.autologin_token(), "token");
        assert!(link_manager.autologin_update_time() >= before);
        assert!(link_manager.autologin_update_time() > stale);
    }
}
