//! `ConnectionCreator` (td/telegram/net/ConnectionCreator.cpp) — the
//! M.3.9.1 slice: the dc-options consumer the ConfigManager and the
//! config recoverer fan out to. The connection machinery itself (the
//! `DcOptionsSet` orderings the link loop consumes, proxies, resolve
//! loop — the actor around `client_loop`/`ping_dc`) is the net step;
//! here the merged option list stands in for the set, in the C++ add
//! order (defaults first, then the sorted update).
//!
//! Ported bodies:
//! - `on_dc_options` (ConnectionCreator.cpp:1012-1037) — the `my_id`
//!   seeded stable sort, the `"dc_options"` binlog save and the set
//!   rebuild;
//! - the `start_up` pmc replay (cpp:1099-1106);
//! - `get_default_dc_options` (cpp:1220-1280), the non-emscripten table.
//! - the proxy registry (`net/Proxy.{h,cpp}` + cpp:117-478): `add_proxy`,
//!   `enable_proxy`/`disable_proxy`/`remove_proxy`/`get_proxies`, the
//!   `init_proxies` pmc restore and the `ping_proxy` validation bands —
//!   the glue slice (the resolve/connect tails are the `[blocked: net]`
//!   seams, the M.31 park convention).

use std::cmp::Ordering;
use std::collections::BTreeMap;

use tddb::binlog_key_value::BinlogKeyValue;
use tdmtproto::proxy_secret::ProxySecret;
use tdutils::hash::hash_int64;
use tdutils::promise::Promise;
use tdutils::utf8::check_utf8;

use crate::config_manager::{DC_OPTION_IPV6, StoredDcOption, StoredDcOptions};
use crate::store_parse::{Parser, store_i32, store_string};
use crate::td_api;

/// The merged option list — the `dc_options_set_` stand-in (see the
/// module docs).
#[derive(Debug)]
pub(crate) struct ConnectionCreator {
    dc_options: Vec<StoredDcOption>,
    /// `proxies_` (cpp:76) — the registry, ordered by id like the C++
    /// `std::map`; the string is each proxy's comment.
    proxies: BTreeMap<i32, (Proxy, String)>,
    /// `active_proxy_id_` — `0` is the no-proxy default.
    active_proxy_id: i32,
    /// `max_proxy_id_` — the C++ in-class `= 2` default (`start_up`
    /// re-pins it).
    max_proxy_id: i32,
    /// `proxy_last_used_date_` / `proxy_last_used_saved_date_`.
    proxy_last_used_date: BTreeMap<i32, i32>,
    proxy_last_used_saved_date: BTreeMap<i32, i32>,
}

impl Default for ConnectionCreator {
    fn default() -> Self {
        Self {
            dc_options: Vec::new(),
            proxies: BTreeMap::new(),
            active_proxy_id: 0,
            max_proxy_id: 2,
            proxy_last_used_date: BTreeMap::new(),
            proxy_last_used_saved_date: BTreeMap::new(),
        }
    }
}

impl ConnectionCreator {
    /// The `start_up` pmc replay (cpp:1099-1106): a parsable
    /// `"dc_options"` blob rebuilds the set directly; an unparsable or
    /// empty store (a fresh database) routes through
    /// `on_dc_options(DcOptions())`, which persists the empty list and
    /// re-adds the defaults.
    pub(crate) fn new(pmc: &mut BinlogKeyValue, is_test_dc: bool) -> Self {
        let serialized = pmc.get_raw(b"dc_options");
        let mut creator = Self::default();
        match StoredDcOptions::parse(&serialized) {
            Ok(stored) => creator.add_dc_options(stored.options, is_test_dc),
            Err(_) => creator.on_dc_options(Vec::new(), 0, is_test_dc, pmc),
        }
        creator
    }

    /// `ConnectionCreator::on_dc_options` (cpp:1012-1037). `seed` is the
    /// `my_id` option (`G()->get_option_integer("my_id")`).
    pub(crate) fn on_dc_options(
        &mut self,
        new_dc_options: Vec<StoredDcOption>,
        seed: i64,
        is_test_dc: bool,
        pmc: &mut BinlogKeyValue,
    ) {
        let mut new_dc_options = new_dc_options;
        new_dc_options.sort_by(|lhs, rhs| {
            if dc_option_less(lhs, rhs, seed) {
                Ordering::Less
            } else if dc_option_less(rhs, lhs, seed) {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        });
        pmc.set_raw(
            b"dc_options",
            &StoredDcOptions {
                options: new_dc_options.clone(),
            }
            .store(),
        );
        self.dc_options.clear(); // dc_options_set_.reset()
        self.add_dc_options(new_dc_options, is_test_dc);
    }

    /// `ConnectionCreator::add_dc_options` (cpp:1039-1048) — the defaults
    /// plus the new options (the `TD_EMSCRIPTEN` half is compiled in).
    fn add_dc_options(&mut self, new_dc_options: Vec<StoredDcOption>, is_test_dc: bool) {
        self.dc_options.extend(get_default_dc_options(is_test_dc));
        self.dc_options.extend(new_dc_options);
    }

    /// The merged options (defaults first, then the last sorted update) —
    /// the net step's `DcOptionsSet` reads replace this.
    #[allow(dead_code)] // the net step's DcOptionsSet reads land; tests pin it now
    pub(crate) fn dc_options(&self) -> &[StoredDcOption] {
        &self.dc_options
    }

    /// The `start_up` proxy half (cpp:1111-1119 + `init_proxies`
    /// cpp:1131-1190): the gate skips the restore only when the store
    /// proves proxies were never added. The Rust keeps it out of
    /// [`Self::new`] because the restore writes the `enabled_proxy_id`
    /// option through the env, and the env needs the `Td` — the caller
    /// runs it right after construction, in the C++ `start_up` order.
    pub(crate) fn restore_proxies(&mut self, env: &mut impl ConnectionCreatorEnv) {
        if env.binlog_pmc_get(b"proxy_max_id") != "2"
            || !env
                .binlog_pmc_get(get_proxy_database_key(1).as_bytes())
                .is_empty()
        {
            // don't need to init proxies if they have never been added
            self.init_proxies(env);
        } else {
            self.max_proxy_id = 2;
        }
    }

    /// `ConnectionCreator::init_proxies` (cpp:1131-1190) — the
    /// `prefix_get("proxy")` restore. The Rust pmc returns full keys, so
    /// the `"proxy"` prefix is stripped here instead of by the C++
    /// `prefix_get` semantics.
    fn init_proxies(&mut self, env: &mut impl ConnectionCreatorEnv) {
        let mut comments: BTreeMap<i32, String> = BTreeMap::new();
        let mut erased_keys: Vec<String> = Vec::new();
        for (key, value) in env.binlog_pmc_prefix_get("proxy") {
            let suffix = key.strip_prefix("proxy").unwrap_or(&key).to_owned();
            match suffix.as_str() {
                "_max_id" => {
                    self.max_proxy_id = suffix_parse(&suffix, &value);
                }
                "_active_id" => {
                    let proxy_id = suffix_parse(&suffix, &value);
                    self.set_active_proxy_id(env, proxy_id, true);
                }
                _ if suffix.starts_with("_used") => {
                    let proxy_id: i32 = suffix[5..].parse().unwrap_or(0);
                    let last_used: i32 = String::from_utf8_lossy(&value).parse().unwrap_or(0);
                    if proxy_id > 0 {
                        self.proxy_last_used_date.insert(proxy_id, last_used);
                        self.proxy_last_used_saved_date.insert(proxy_id, last_used);
                    }
                }
                _ if suffix.starts_with("_comment") => {
                    let proxy_id: i32 = suffix[8..].parse().unwrap_or(0);
                    if proxy_id > 0 {
                        comments.insert(proxy_id, String::from_utf8_lossy(&value).into_owned());
                    }
                }
                _ => {
                    let proxy_id: i32 = if suffix.is_empty() {
                        1
                    } else {
                        suffix.parse().unwrap_or(0)
                    };
                    if proxy_id <= 0 || self.proxies.contains_key(&proxy_id) {
                        continue;
                    }
                    let mut proxy = Proxy::default();
                    // `log_event_store` framing (ConnectionCreator.cpp:202):
                    // the log-event version prefix leads the record.
                    let payload = value
                        .strip_prefix(
                            crate::store_parse::version::LOG_EVENT_STORE_VERSION
                                .to_le_bytes()
                                .as_slice(),
                        )
                        .unwrap_or(&value);
                    if let Err(error) = proxy.parse(payload) {
                        tdutils::log_error!("Can't parse proxy {proxy_id}: {error}");
                        continue;
                    }
                    if proxy.type_ == ProxyType::None {
                        // `LOG_IF(ERROR, proxy_id != 1)` + the erase walk
                        // (cpp:1179-1189): an empty stored record is dropped.
                        if proxy_id != 1 {
                            tdutils::log_error!("Have empty proxy {proxy_id}");
                        }
                        erased_keys.push(get_proxy_database_key(proxy_id));
                        erased_keys.push(get_proxy_comment_database_key(proxy_id));
                        erased_keys.push(get_proxy_used_database_key(proxy_id));
                        self.proxies.remove(&proxy_id);
                        if self.active_proxy_id == proxy_id {
                            self.set_active_proxy_id(env, 0, false);
                        }
                    } else {
                        self.proxies.insert(proxy_id, (proxy, String::new()));
                    }
                }
            }
        }
        for key in erased_keys {
            env.binlog_pmc_erase(&key);
        }
        for (proxy_id, comment) in comments {
            // The C++ `proxies_[proxy_id].second` operator[] default-inserts
            // a missing entry — mirrored with the same fallback.
            let entry = self.proxies.entry(proxy_id).or_default();
            set_proxy_comment(env, proxy_id, &mut entry.1, comment);
        }
    }

    /// `ConnectionCreator::add_proxy` (cpp:158-212) — the edit-dedup
    /// ladder and the id allocation. `old_proxy_id < 0` adds; the
    /// TD_EMSCRIPTEN/WATCH_OS unsupported-platform band is compiled out
    /// on this line like in the C++.
    pub(crate) fn add_proxy(
        &mut self,
        env: &mut impl ConnectionCreatorEnv,
        old_proxy_id: i32,
        proxy: Option<&td_api::Proxy>,
        enable: bool,
        comment: String,
    ) -> Result<td_api::AddedProxy, String> {
        let new_proxy = Proxy::create_proxy(proxy)?;
        let mut enable = enable;
        if old_proxy_id >= 0 {
            let Some((old_proxy, old_comment)) = self.proxies.get_mut(&old_proxy_id) else {
                return Err("Proxy not found".to_string());
            };
            if *old_proxy == new_proxy {
                set_proxy_comment(env, old_proxy_id, old_comment, comment);
                if enable {
                    self.enable_proxy_impl(env, old_proxy_id);
                }
                return Ok(self.get_added_proxy_object(old_proxy_id));
            }
            if old_proxy_id == self.active_proxy_id {
                enable = true;
                self.disable_proxy_impl(env);
            }

            self.proxies.remove(&old_proxy_id);
            env.binlog_pmc_erase(&get_proxy_comment_database_key(old_proxy_id));
            env.binlog_pmc_erase(&get_proxy_used_database_key(old_proxy_id));
            self.proxy_last_used_date.remove(&old_proxy_id);
            self.proxy_last_used_saved_date.remove(&old_proxy_id);
        }

        let proxy_id = {
            let mut found = None;
            for (id, (proxy, _)) in &self.proxies {
                if *proxy == new_proxy {
                    found = Some(*id);
                    break;
                }
            }
            match found {
                Some(proxy_id) => proxy_id,
                None => {
                    let mut proxy_id = old_proxy_id;
                    if proxy_id < 0 {
                        debug_assert!(self.max_proxy_id >= 2);
                        proxy_id = self.max_proxy_id;
                        self.max_proxy_id += 1;
                        env.binlog_pmc_set_raw(
                            b"proxy_max_id",
                            self.max_proxy_id.to_string().as_bytes(),
                        );
                    }
                    // `log_event_store` framing — the version prefix leads
                    // the record.
                    let mut value = Vec::new();
                    crate::store_parse::store_log_event_version(&mut value);
                    value.extend_from_slice(&new_proxy.store());
                    env.binlog_pmc_set_raw(get_proxy_database_key(proxy_id).as_bytes(), &value);
                    let is_inserted = self
                        .proxies
                        .insert(proxy_id, (new_proxy, String::new()))
                        .is_none();
                    debug_assert!(is_inserted);
                    proxy_id
                }
            }
        };
        let comment_stored = self.proxies.get_mut(&proxy_id).expect("proxy inserted");
        set_proxy_comment(env, proxy_id, &mut comment_stored.1, comment);
        if enable {
            self.enable_proxy_impl(env, proxy_id);
        }
        Ok(self.get_added_proxy_object(proxy_id))
    }

    /// `ConnectionCreator::enable_proxy` (cpp:214-221).
    pub(crate) fn enable_proxy(
        &mut self,
        env: &mut impl ConnectionCreatorEnv,
        proxy_id: i32,
    ) -> Result<(), String> {
        if !self.proxies.contains_key(&proxy_id) {
            return Err("Unknown proxy identifier".to_string());
        }

        self.enable_proxy_impl(env, proxy_id);
        Ok(())
    }

    /// `ConnectionCreator::disable_proxy` (cpp:223-227).
    pub(crate) fn disable_proxy(&mut self, env: &mut impl ConnectionCreatorEnv) {
        self.save_proxy_last_used_date(env, 0);
        self.disable_proxy_impl(env);
    }

    /// `ConnectionCreator::remove_proxy` (cpp:229-244).
    pub(crate) fn remove_proxy(
        &mut self,
        env: &mut impl ConnectionCreatorEnv,
        proxy_id: i32,
    ) -> Result<(), String> {
        if !self.proxies.contains_key(&proxy_id) {
            return Err("Unknown proxy identifier".to_string());
        }

        if proxy_id == self.active_proxy_id {
            self.disable_proxy_impl(env);
        }

        self.proxies.remove(&proxy_id);

        env.binlog_pmc_erase(&get_proxy_database_key(proxy_id));
        env.binlog_pmc_erase(&get_proxy_comment_database_key(proxy_id));
        env.binlog_pmc_erase(&get_proxy_used_database_key(proxy_id));
        Ok(())
    }

    /// `ConnectionCreator::get_proxies` (cpp:246-254) — the registry
    /// order (the C++ `std::map` iterates by ascending id).
    pub(crate) fn get_proxies(&self) -> td_api::AddedProxies {
        td_api::AddedProxies {
            proxies: self
                .proxies
                .keys()
                .copied()
                .map(|proxy_id| self.get_added_proxy_object(proxy_id))
                .collect(),
        }
    }

    /// `ConnectionCreator::ping_proxy` (cpp:278-334) — the given-proxy
    /// path only: the C++ null-input path pings the main DC through the
    /// `DcOptionsSet` connection list, and the flattened td_api model
    /// makes `request.proxy` non-nullable, so that band is inexpressible
    /// here. The validation bands are exact; the resolve/connect tail is
    /// the `[blocked: net]` seam — the promise rides the seam and resolves
    /// with the C++ LambdaPromise drop ("Lost promise") until the net
    /// step's ping machinery answers it.
    pub(crate) fn ping_proxy(
        &mut self,
        env: &mut impl ConnectionCreatorEnv,
        input_proxy: Option<&td_api::Proxy>,
        promise: Promise<f64>,
    ) -> Result<(), String> {
        let proxy = Proxy::create_proxy(input_proxy)?;
        let _prefer_ipv6 = env.get_option_boolean("prefer_ipv6", false);
        env.ping_via_net(&proxy, promise);
        Ok(())
    }

    /// `ConnectionCreator::enable_proxy_impl` (cpp:403-418).
    fn enable_proxy_impl(&mut self, env: &mut impl ConnectionCreatorEnv, proxy_id: i32) {
        debug_assert!(self.proxies.contains_key(&proxy_id));
        if proxy_id == self.active_proxy_id {
            return;
        }

        let active_is_mtproto = self
            .proxies
            .get(&self.active_proxy_id)
            .is_some_and(|(proxy, _)| proxy.use_mtproto_transport_proxy());
        let new_is_mtproto = self.proxies[&proxy_id].0.use_mtproto_transport_proxy();
        if (self.active_proxy_id != 0 && active_is_mtproto) || new_is_mtproto {
            env.update_mtproto_header(Some(&self.proxies[&proxy_id].0));
        }
        self.save_proxy_last_used_date(env, 0);

        self.set_active_proxy_id(env, proxy_id, false);

        self.on_proxy_changed(env, false);
    }

    /// `ConnectionCreator::disable_proxy_impl` (cpp:420-435).
    fn disable_proxy_impl(&mut self, env: &mut impl ConnectionCreatorEnv) {
        if self.active_proxy_id == 0 {
            env.remove_sponsored_dialog();
            env.reload_promo_data();
            return;
        }
        debug_assert!(self.proxies.contains_key(&self.active_proxy_id));

        if self.proxies[&self.active_proxy_id]
            .0
            .use_mtproto_transport_proxy()
        {
            env.update_mtproto_header(None);
        }

        self.set_active_proxy_id(env, 0, false);

        self.on_proxy_changed(env, false);
    }

    /// `ConnectionCreator::on_proxy_changed` (cpp:457-484) — the state
    /// flag and the resolve/children resets are the net step's consumers
    /// (the `[blocked: net]` seams); the promo fan-outs, the flag itself
    /// and the web-proxy-carrier fan-out are real.
    fn on_proxy_changed(&mut self, env: &mut impl ConnectionCreatorEnv, from_db: bool) {
        let use_proxy = self.active_proxy_id != 0
            && self
                .proxies
                .get(&self.active_proxy_id)
                .is_some_and(|(proxy, _)| {
                    proxy.type_ != ProxyType::Mtproto && proxy.type_ != ProxyType::HttpCaching
                });
        env.on_proxy_state(use_proxy);

        // the manager opens a carrier as soon as a web proxy is enabled,
        // and closes it when the proxy changes (cpp:463-465); the manager
        // filters `use_web_proxy` itself
        let (send_proxy_id, send_proxy) = if self.active_proxy_id == 0 {
            (0, Proxy::default())
        } else {
            let proxy = &self.proxies[&self.active_proxy_id].0;
            (self.active_proxy_id, proxy.clone())
        };
        env.web_proxy_set_active_proxy(send_proxy_id, send_proxy);

        if !from_db {
            // the children `send_closure` resets — the net step's
            // per-client session drop
        }

        // `VLOG(connections) << "Drop proxy IP address"` + the resolve
        // state reset — the resolve loop is the net step.
        if self.active_proxy_id == 0 || !from_db {
            env.remove_sponsored_dialog();
        }
        env.reload_promo_data();

        // `loop()` — the net step's link loop.
    }

    /// `ConnectionCreator::set_active_proxy_id` (cpp:386-401) — the
    /// option write happens on every path; the pmc write and the config
    /// refetch only off the binlog.
    fn set_active_proxy_id(
        &mut self,
        env: &mut impl ConnectionCreatorEnv,
        proxy_id: i32,
        from_binlog: bool,
    ) {
        self.active_proxy_id = proxy_id;
        if proxy_id == 0 {
            env.set_option_empty("enabled_proxy_id");
        } else {
            env.set_option_integer("enabled_proxy_id", i64::from(proxy_id));
        }
        if !from_binlog {
            if proxy_id == 0 {
                env.binlog_pmc_erase("proxy_active_id");
                env.request_config(false);
            } else {
                env.binlog_pmc_set_raw(b"proxy_active_id", proxy_id.to_string().as_bytes());
            }
        }
    }

    /// `ConnectionCreator::save_proxy_last_used_date` (cpp:481-496).
    fn save_proxy_last_used_date(&mut self, env: &mut impl ConnectionCreatorEnv, delay: i32) {
        if self.active_proxy_id == 0 {
            return;
        }

        debug_assert!(delay >= 0);
        let date = self
            .proxy_last_used_date
            .entry(self.active_proxy_id)
            .or_default();
        if *date
            <= *self
                .proxy_last_used_saved_date
                .entry(self.active_proxy_id)
                .or_default()
                + delay
        {
            return;
        }

        let saved = *date;
        *self
            .proxy_last_used_saved_date
            .get_mut(&self.active_proxy_id)
            .unwrap() = saved;
        env.binlog_pmc_set_raw(
            get_proxy_used_database_key(self.active_proxy_id).as_bytes(),
            saved.to_string().as_bytes(),
        );
    }

    /// `ConnectionCreator::get_added_proxy_object` (cpp:498-505).
    fn get_added_proxy_object(&self, proxy_id: i32) -> td_api::AddedProxy {
        let (proxy, comment) = &self.proxies[&proxy_id];
        let last_used_date = self
            .proxy_last_used_date
            .get(&proxy_id)
            .copied()
            .unwrap_or(0);
        td_api::AddedProxy {
            id: proxy_id,
            last_used_date,
            is_enabled: proxy_id == self.active_proxy_id,
            comment: comment.clone(),
            proxy: Some(proxy.get_proxy_object()),
        }
    }
}

/// The `init_proxies` integer-read helper: the C++
/// `to_integer_safe`/`to_integer` pair — the `_max_id`/`_active_id`
/// values are trusted (they were written by this code), the id suffixes
/// parse with a 0 fallback the caller filters.
fn suffix_parse(_suffix: &str, value: &[u8]) -> i32 {
    String::from_utf8_lossy(value).parse().unwrap_or(0)
}

/// [`ConnectionCreator`] seams over the `Td` fields — the env
/// convention ([`crate::background_manager::BackgroundManagerEnv`]).
pub(crate) trait ConnectionCreatorEnv {
    /// `G()->td_db()->get_binlog_pmc()->get` — the decimal-string view.
    fn binlog_pmc_get(&mut self, key: &[u8]) -> String;

    /// `G()->td_db()->get_binlog_pmc()->set` (the decimal-string form).
    fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]);

    /// `G()->td_db()->get_binlog_pmc()->erase`.
    fn binlog_pmc_erase(&mut self, key: &str);

    /// `G()->td_db()->get_binlog_pmc()->prefix_get`.
    fn binlog_pmc_prefix_get(&mut self, prefix: &str) -> Vec<(String, Vec<u8>)>;

    /// `G()->get_option_boolean`.
    fn get_option_boolean(&self, name: &str, default_value: bool) -> bool;

    /// `G()->set_option_integer` / `set_option_empty` — the
    /// `enabled_proxy_id` writes.
    fn set_option_integer(&mut self, name: &str, value: i64);

    fn set_option_empty(&mut self, name: &str);

    /// `send_closure(G()->config_manager(), &ConfigManager::request_config,
    /// false)` — the config refetch after the proxy went away.
    fn request_config(&mut self, reopen_sessions: bool);

    /// `PromoDataManager::remove_sponsored_dialog`.
    fn remove_sponsored_dialog(&mut self);

    /// `PromoDataManager::reload_promo_data`.
    fn reload_promo_data(&mut self);

    /// `ConnectionCreator::update_mtproto_header` — the net step's
    /// session/mtproto-header rebuild; `None` is the C++ `Proxy()`.
    fn update_mtproto_header(&mut self, proxy: Option<&Proxy>);

    /// `StateManager::on_proxy` — the collapsed state world's flag sink.
    fn on_proxy_state(&mut self, use_proxy: bool);

    /// `GetHostByNameActor::run` + `ping_proxy_resolved` — the
    /// `[blocked: net]` resolve/connect seam; the promise resolves with
    /// the C++ LambdaPromise drop until the net step answers it.
    fn ping_via_net(&mut self, proxy: &Proxy, promise: Promise<f64>);

    /// `ProxyChecker::test_proxy` — the `[blocked: net]` handshake seam.
    fn test_proxy_via_net(&mut self, proxy: &Proxy, dc_id: i32, timeout: f64, promise: Promise<()>);

    /// `send_closure(G()->web_proxy_manager(), &WebProxyManager::
    /// set_active_proxy, ...)` (cpp:463-465) — the carrier open/close
    /// fan-out; the manager filters `use_web_proxy` itself.
    fn web_proxy_set_active_proxy(&mut self, proxy_id: i32, proxy: Proxy);
}

/// `ConnectionCreator::set_proxy_comment` (cpp:143-156).
fn set_proxy_comment(
    env: &mut impl ConnectionCreatorEnv,
    proxy_id: i32,
    comment: &mut String,
    new_comment: String,
) {
    if comment == &new_comment {
        return;
    }
    *comment = new_comment;
    debug_assert!(proxy_id > 0);
    if comment.is_empty() {
        env.binlog_pmc_erase(&get_proxy_comment_database_key(proxy_id));
    } else {
        env.binlog_pmc_set_raw(
            get_proxy_comment_database_key(proxy_id).as_bytes(),
            comment.as_bytes(),
        );
    }
}

/// `ConnectionCreator::get_proxy_database_key` (cpp:460-468) — proxy 1
/// keeps the bare `"proxy"` key (the C++ store compat).
fn get_proxy_database_key(proxy_id: i32) -> String {
    debug_assert!(proxy_id > 0);
    if proxy_id == 1 {
        return "proxy".to_string();
    }
    format!("proxy{proxy_id}")
}

/// `ConnectionCreator::get_proxy_comment_database_key` (cpp:470-472).
fn get_proxy_comment_database_key(proxy_id: i32) -> String {
    debug_assert!(proxy_id > 0);
    format!("proxy_comment{proxy_id}")
}

/// `ConnectionCreator::get_proxy_used_database_key` (cpp:474-478).
fn get_proxy_used_database_key(proxy_id: i32) -> String {
    debug_assert!(proxy_id > 0);
    format!("proxy_used{proxy_id}")
}

/// `Proxy::Type` (net/Proxy.h:96-99) — the i32 discriminants are the
/// stored form.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ProxyType {
    #[default]
    None = 0,
    Socks5 = 1,
    Mtproto = 2,
    HttpTcp = 3,
    HttpCaching = 4,
    Web = 5,
}

/// `Proxy` (net/Proxy.h) — the td_api-level proxy record with the exact
/// tl_helpers store/parse (the `log_event_store` bytes).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Proxy {
    pub type_: ProxyType,
    pub server: String,
    pub port: i32,
    pub user: String,
    pub password: String,
    pub path: String,
    pub secret: ProxySecret,
}

impl Proxy {
    /// `Proxy::create_proxy(const td_api::proxy *)` (Proxy.cpp:50-56) —
    /// the null-proxy band; the flattened td_api makes the caller's
    /// `request.proxy` non-nullable, so only `None` input (the C++ null
    /// pointer at the `ProxyType` level is likewise flattened away)
    /// reaches this. Kept for the option-taking arms.
    pub(crate) fn create_proxy(proxy: Option<&td_api::Proxy>) -> Result<Proxy, String> {
        let Some(proxy) = proxy else {
            return Err("Proxy must be non-empty".to_string());
        };
        Self::create_proxy_parts(&proxy.server, proxy.port, proxy.r#type.as_ref())
    }

    /// `Proxy::create_proxy(string, int, const td_api::ProxyType *)`
    /// (Proxy.cpp:12-48) — the exact 400 ladder.
    pub(crate) fn create_proxy_parts(
        server: &str,
        port: i32,
        proxy_type: Option<&td_api::ProxyType>,
    ) -> Result<Proxy, String> {
        let Some(proxy_type) = proxy_type else {
            return Err("Proxy type must be non-empty".to_string());
        };
        // `Proxy::create_proxy` (net/Proxy.cpp:159-173) — the web arm rides
        // before the server/port validation ladder: the port must be 0 (the
        // forced 443 convention) or exactly the web-proxy port.
        if let td_api::ProxyType::proxyTypeWeb(type_web) = proxy_type {
            if port != 0 && port != Self::WEB_PROXY_PORT {
                return Err("Web proxy port must be 443".to_string());
            }
            let host = Self::normalize_web_proxy_host(server)?;
            if !Self::is_valid_web_proxy_path(&type_web.path) {
                return Err("Invalid web proxy path".to_string());
            }
            let secret = Self::get_web_proxy_secret(&type_web.secret)?;
            return Ok(Self::web(host, type_web.path.clone(), secret));
        }
        if server.is_empty() {
            return Err("Server name must be non-empty".to_string());
        }
        if server.len() > 255 {
            return Err("Server name is too long".to_string());
        }
        if !check_utf8(server.as_bytes()) {
            return Err("Server name must be encoded in UTF-8".to_string());
        }
        if port <= 0 || port > 65535 {
            return Err("Wrong port number".to_string());
        }

        match proxy_type {
            td_api::ProxyType::proxyTypeSocks5(type_) => Ok(Self {
                type_: ProxyType::Socks5,
                server: server.to_string(),
                port,
                user: type_.username.clone(),
                password: type_.password.clone(),
                path: String::new(),
                secret: ProxySecret::default(),
            }),
            td_api::ProxyType::proxyTypeHttp(type_) => {
                let proxy_type = if type_.http_only {
                    ProxyType::HttpCaching
                } else {
                    ProxyType::HttpTcp
                };
                Ok(Self {
                    type_: proxy_type,
                    server: server.to_string(),
                    port,
                    user: type_.username.clone(),
                    password: type_.password.clone(),
                    path: String::new(),
                    secret: ProxySecret::default(),
                })
            }
            td_api::ProxyType::proxyTypeMtproto(type_) => {
                let secret = match ProxySecret::from_link(type_.secret.as_bytes(), false) {
                    tdutils::status::Result::Ok(secret) => secret,
                    tdutils::status::Result::Err(status) => {
                        return Err(status.message().to_string());
                    }
                };
                Ok(Self {
                    type_: ProxyType::Mtproto,
                    server: server.to_string(),
                    port,
                    user: String::new(),
                    password: String::new(),
                    path: String::new(),
                    secret,
                })
            }
            // `Proxy::create_proxy`'s UNREACHABLE default (net/Proxy.cpp:
            // 206-208) — the web arm is handled before the validation
            // ladder; the flattened td_api keeps every ProxyType variant
            // matched at compile time.
            td_api::ProxyType::proxyTypeWeb(_) => {
                unreachable!("the web arm is handled before the validation ladder")
            }
        }
    }

    /// `Proxy::WEB_PROXY_PORT` (net/Proxy.h:87).
    pub(crate) const WEB_PROXY_PORT: i32 = 443;

    /// `Proxy::web` (net/Proxy.h:89-97) — the port is forced to 443.
    pub(crate) fn web(server: String, path: String, secret: ProxySecret) -> Proxy {
        Proxy {
            type_: ProxyType::Web,
            server,
            port: Self::WEB_PROXY_PORT,
            path,
            secret,
            ..Proxy::default()
        }
    }

    /// `Proxy::use_web_proxy` (net/Proxy.h:147-149).
    pub(crate) fn use_web_proxy(&self) -> bool {
        self.type_ == ProxyType::Web
    }

    /// `Proxy::get_web_proxy_bridge_capability` (net/Proxy.cpp:165-177) —
    /// the HMAC capability the relay page checks; the "tdesktop-" prefix is
    /// part of the contract (the deployed relay checks these exact bytes),
    /// so it must not depend on the OS.
    pub(crate) fn get_web_proxy_bridge_capability(&self) -> String {
        debug_assert!(self.type_ == ProxyType::Web);
        let context = if self.path.is_empty() {
            format!("tdesktop-web-proxy-bridge-v1\n{}", self.server)
        } else {
            format!(
                "tdesktop-web-proxy-bridge-v2\n{}\n{}",
                self.server, self.path
            )
        };
        let hash = tdutils::crypto::hmac_sha256(self.secret.get_raw_secret(), context.as_bytes());
        String::from_utf8_lossy(&tdmtproto::utils::base64url_encode(&hash)).into_owned()
    }

    /// `Proxy::use_mtproto_transport_proxy` (net/Proxy.h:151-153) — an
    /// MTProto proxy, which is reached directly or through a web proxy
    /// relay.
    fn use_mtproto_transport_proxy(&self) -> bool {
        self.type_ == ProxyType::Mtproto || self.type_ == ProxyType::Web
    }

    /// `Proxy::normalize_web_proxy_host` (net/Proxy.cpp:16-72) — the
    /// canonical lower-case ASCII host; the WHATWG "ends in a number"
    /// rule keeps IPv4 shorthand hosts out.
    pub(crate) fn normalize_web_proxy_host(server: &str) -> Result<String, String> {
        const INVALID_HOST: &str = "Invalid web proxy server";
        if !check_utf8(server.as_bytes()) {
            return Err(INVALID_HOST.to_string());
        }
        let host = ascii_trim(server);
        const SCHEME: &str = "https://";
        let mut host = host.as_bytes();
        if host.len() >= SCHEME.len()
            && host[..SCHEME.len()].eq_ignore_ascii_case(SCHEME.as_bytes())
        {
            host = &host[SCHEME.len()..];
        }
        if host.last() == Some(&b'.') {
            host = &host[..host.len() - 1];
        }
        if host.is_empty() || host.len() > 253 * 4 {
            return Err(INVALID_HOST.to_string());
        }
        let host = String::from_utf8_lossy(host);
        // only the Windows idn_to_ascii case-folds, but the host is a part
        // of the capability, so it must not depend on the OS — the
        // lower-case form rides in before the punycode pass like the C++
        // `idn_to_ascii(utf8_to_lower(host))`.
        let lowered = host.to_lowercase();
        let result = tdnet::dns::idn_to_ascii(&lowered).map_err(|_| INVALID_HOST.to_string())?;
        let result = result.to_lowercase();
        if result.len() > 253 {
            return Err(INVALID_HOST.to_string());
        }
        // `td::full_split(result, '.')` — an empty label fails the same
        // loop as an over-long one.
        let labels: Vec<&str> = result.split('.').collect();
        if labels.len() < 2 {
            return Err(INVALID_HOST.to_string());
        }
        for label in &labels {
            if label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
            {
                return Err(INVALID_HOST.to_string());
            }
            for &c in label.as_bytes() {
                if !c.is_ascii_alphanumeric() && c != b'-' {
                    return Err(INVALID_HOST.to_string());
                }
            }
        }

        // the WHATWG URL "ends in a number" rule: such a host is an IPv4
        // address, possibly in a shorthand like 127.1 or 0x7f.1
        let mut last_label = labels[labels.len() - 1];
        let is_hex = last_label.starts_with("0x");
        if is_hex {
            last_label = &last_label[2..];
        }
        let is_number = last_label.bytes().all(|c| {
            if is_hex {
                c.is_ascii_hexdigit()
            } else {
                c.is_ascii_digit()
            }
        });
        if is_number {
            return Err("Web proxy server must be a domain name".to_string());
        }
        Ok(result)
    }

    /// `Proxy::is_valid_web_proxy_path` (net/Proxy.cpp:74-98).
    pub(crate) fn is_valid_web_proxy_path(path: &str) -> bool {
        if path.len() > 128 {
            return false;
        }
        let mut is_segment_start = true;
        for &c in path.as_bytes() {
            if c == b'/' {
                if is_segment_start {
                    return false;
                }
                is_segment_start = true;
                continue;
            }
            if !c.is_ascii_alphanumeric() && (is_segment_start || (c != b'-' && c != b'_')) {
                return false;
            }
            is_segment_start = false;
        }
        path.is_empty() || !is_segment_start
    }

    /// `Proxy::parse_web_proxy_address` (net/Proxy.cpp:100-118) — returns
    /// the canonical host and path of a web proxy address "host[/path]".
    pub(crate) fn parse_web_proxy_address(address: &str) -> Result<(String, String), String> {
        let address = ascii_trim(address);
        const SCHEME: &str = "https://";
        let mut address = address.as_bytes();
        if address.len() >= SCHEME.len()
            && address[..SCHEME.len()].eq_ignore_ascii_case(SCHEME.as_bytes())
        {
            address = &address[SCHEME.len()..];
        }
        let (host, path) = match address.iter().position(|&c| c == b'/') {
            Some(slash_pos) => (&address[..slash_pos], &address[slash_pos + 1..]),
            None => (address, &b""[..]),
        };
        let normalized_host = Self::normalize_web_proxy_host(&String::from_utf8_lossy(host))?;
        let path = String::from_utf8_lossy(path);
        if !Self::is_valid_web_proxy_path(&path) {
            return Err("Invalid web proxy path".to_string());
        }
        Ok((normalized_host, path.into_owned()))
    }

    /// `Proxy::get_web_proxy_secret` (net/Proxy.cpp:120-128) — the relay
    /// forwards raw obfuscated bytes, so a fake-TLS secret would make the
    /// MTProxy wait for a TLS handshake that never comes.
    pub(crate) fn get_web_proxy_secret(encoded_secret: &str) -> Result<ProxySecret, String> {
        let secret = match ProxySecret::from_link(encoded_secret.as_bytes(), false) {
            tdutils::status::Result::Ok(secret) => secret,
            tdutils::status::Result::Err(status) => {
                return Err(status.message().to_string());
            }
        };
        if secret.emulate_tls() {
            return Err("Web proxies don't support fake TLS secrets".to_string());
        }
        Ok(secret)
    }

    /// `Proxy::get_web_proxy_link_secret` (net/Proxy.cpp:130-145) — a link
    /// secret of a web proxy with a path is marked with the 0x70 byte, so
    /// that clients without path support reject the link.
    pub(crate) fn get_web_proxy_link_secret(
        link_secret: &str,
        has_path: bool,
    ) -> Result<ProxySecret, String> {
        // 0xdd must never be used as the marker: older clients read a
        // 17-byte secret starting with it as a valid one
        const WEB_PROXY_LINK_SECRET_MARKER: u8 = 0x70;
        if let tdutils::status::Result::Ok(marked) =
            tdmtproto::utils::base64url_decode(link_secret.as_bytes())
            && marked.len() >= 17
            && marked[0] == WEB_PROXY_LINK_SECRET_MARKER
            && tdmtproto::utils::base64url_encode(&marked) == link_secret.as_bytes()
            && let tdutils::status::Result::Ok(secret) =
                ProxySecret::from_binary(&marked[1..], false)
            && !secret.emulate_tls()
        {
            return Ok(secret);
        }
        if has_path {
            return Err("Wrong proxy secret".to_string());
        }
        Self::get_web_proxy_secret(link_secret)
    }

    /// `Proxy::get_proxy_object` (Proxy.cpp:58-78).
    pub(crate) fn get_proxy_object(&self) -> td_api::Proxy {
        let r#type = match self.type_ {
            ProxyType::Socks5 => td_api::ProxyType::proxyTypeSocks5(td_api::proxyTypeSocks5 {
                username: self.user.clone(),
                password: self.password.clone(),
            }),
            ProxyType::HttpTcp => td_api::ProxyType::proxyTypeHttp(td_api::proxyTypeHttp {
                username: self.user.clone(),
                password: self.password.clone(),
                http_only: false,
            }),
            ProxyType::HttpCaching => td_api::ProxyType::proxyTypeHttp(td_api::proxyTypeHttp {
                username: self.user.clone(),
                password: self.password.clone(),
                http_only: true,
            }),
            ProxyType::Mtproto => td_api::ProxyType::proxyTypeMtproto(td_api::proxyTypeMtproto {
                secret: String::from_utf8_lossy(&self.secret.get_encoded_secret()).into_owned(),
            }),
            ProxyType::Web => td_api::ProxyType::proxyTypeWeb(td_api::proxyTypeWeb {
                path: self.path.clone(),
                secret: String::from_utf8_lossy(&self.secret.get_encoded_secret()).into_owned(),
            }),
            ProxyType::None => unreachable!("the C++ UNREACHABLE default"),
        };
        td_api::Proxy {
            server: self.server.clone(),
            port: self.port,
            r#type: Some(r#type),
        }
    }

    /// `Proxy::store` (Proxy.h:131-149) — the tl_helpers bytes: the
    /// `Type` int32, then the socks/http or mtproto field groups.
    pub(crate) fn store(&self) -> Vec<u8> {
        let mut out = Vec::new();
        store_i32(&mut out, self.type_ as i32);
        match self.type_ {
            ProxyType::Socks5 | ProxyType::HttpTcp | ProxyType::HttpCaching => {
                store_string(&mut out, &self.server);
                store_i32(&mut out, self.port);
                store_string(&mut out, &self.user);
                store_string(&mut out, &self.password);
            }
            ProxyType::Mtproto => {
                store_string(&mut out, &self.server);
                store_i32(&mut out, self.port);
                store_string(
                    &mut out,
                    &String::from_utf8_lossy(&self.secret.get_encoded_secret()),
                );
            }
            ProxyType::Web => {
                store_string(&mut out, &self.server);
                store_i32(&mut out, self.port);
                store_string(
                    &mut out,
                    &String::from_utf8_lossy(&self.secret.get_encoded_secret()),
                );
                store_string(&mut out, &self.path);
            }
            ProxyType::None => {}
        }
        out
    }

    /// `Proxy::parse` (Proxy.h:151-171) — the mtproto secret re-parses
    /// with `truncate_if_needed = true` like the C++.
    pub(crate) fn parse(&mut self, data: &[u8]) -> Result<(), String> {
        let mut parser = Parser::new(data);
        self.type_ = Self::parse_type(parser.i32_().map_err(|e| e.to_string())?)?;
        match self.type_ {
            ProxyType::Socks5 | ProxyType::HttpTcp | ProxyType::HttpCaching => {
                self.server = parser.string().map_err(|e| e.to_string())?;
                self.port = parser.i32_().map_err(|e| e.to_string())?;
                self.user = parser.string().map_err(|e| e.to_string())?;
                self.password = parser.string().map_err(|e| e.to_string())?;
            }
            ProxyType::Mtproto => {
                self.server = parser.string().map_err(|e| e.to_string())?;
                self.port = parser.i32_().map_err(|e| e.to_string())?;
                let encoded = parser.string().map_err(|e| e.to_string())?;
                self.secret = match ProxySecret::from_link(encoded.as_bytes(), true) {
                    tdutils::status::Result::Ok(secret) => secret,
                    tdutils::status::Result::Err(status) => {
                        return Err(status.message().to_string());
                    }
                };
            }
            ProxyType::Web => {
                self.server = parser.string().map_err(|e| e.to_string())?;
                self.port = parser.i32_().map_err(|e| e.to_string())?;
                let encoded = parser.string().map_err(|e| e.to_string())?;
                self.secret = match ProxySecret::from_link(encoded.as_bytes(), true) {
                    tdutils::status::Result::Ok(secret) => secret,
                    tdutils::status::Result::Err(status) => {
                        return Err(status.message().to_string());
                    }
                };
                self.path = parser.string().map_err(|e| e.to_string())?;
            }
            ProxyType::None => {}
        }
        Ok(())
    }

    /// The `Type` int32 back-reference (the C++ parse reads the enum
    /// value straight; unknown values are the C++ `CHECK` failure — the
    /// Rust answers a parse error instead of aborting the process).
    fn parse_type(value: i32) -> Result<ProxyType, String> {
        match value {
            0 => Ok(ProxyType::None),
            1 => Ok(ProxyType::Socks5),
            2 => Ok(ProxyType::Mtproto),
            3 => Ok(ProxyType::HttpTcp),
            4 => Ok(ProxyType::HttpCaching),
            5 => Ok(ProxyType::Web),
            _ => Err(format!("Unknown proxy type {value}")),
        }
    }
}

/// `td::trim` (td/utils/misc.cpp) — the ASCII space set, not the Unicode
/// one, to keep the byte-level behavior of the host/path normalization.
fn ascii_trim(s: &str) -> String {
    s.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r'))
        .to_owned()
}

/// `NetType` (net/NetType.h:17) — the i8 discriminants are the C++ enum
/// values (`Size`/`Unknown` included for completeness).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)] // Size/Unknown ride the C++ enum; the arms use the rest
pub(crate) enum NetType {
    #[default]
    Other = 0,
    WiFi = 1,
    Mobile = 2,
    MobileRoaming = 3,
    Size = 4,
    None = 5,
    Unknown = 6,
}

/// `get_net_type` (net/NetType.h:19-39) — the C++ null input answers
/// `NetType::Other`.
pub(crate) fn get_net_type(net_type: Option<&td_api::NetworkType>) -> NetType {
    let Some(net_type) = net_type else {
        return NetType::Other;
    };
    match net_type {
        td_api::NetworkType::networkTypeOther(_) => NetType::Other,
        td_api::NetworkType::networkTypeWiFi(_) => NetType::WiFi,
        td_api::NetworkType::networkTypeMobile(_) => NetType::Mobile,
        td_api::NetworkType::networkTypeMobileRoaming(_) => NetType::MobileRoaming,
        td_api::NetworkType::networkTypeNone(_) => NetType::None,
    }
}

/// `get_network_type_object` (net/NetType.h:41-56) — the stats/
/// auto-download consumers land with their slices.
#[allow(dead_code)]
pub(crate) fn get_network_type_object(net_type: NetType) -> td_api::NetworkType {
    match net_type {
        NetType::Other => td_api::NetworkType::networkTypeOther(td_api::networkTypeOther {}),
        NetType::WiFi => td_api::NetworkType::networkTypeWiFi(td_api::networkTypeWiFi {}),
        NetType::Mobile => td_api::NetworkType::networkTypeMobile(td_api::networkTypeMobile {}),
        NetType::MobileRoaming => {
            td_api::NetworkType::networkTypeMobileRoaming(td_api::networkTypeMobileRoaming {})
        }
        _ => unreachable!("the C++ UNREACHABLE default"),
    }
}

/// The `std::stable_sort` comparator (cpp:1014-1034). The ipv4 hash
/// tie-break keeps the order stable across restarts while spreading the
/// load over addresses of one DC.
fn dc_option_less(lhs: &StoredDcOption, rhs: &StoredDcOption, seed: i64) -> bool {
    // `DcId::operator<`/`operator==` (net/DcId.h:75-80): the
    // (dc_id_, is_external_) pair — a CDN option sorts after an internal
    // one with the same id.
    let lhs_dc = (lhs.raw_dc_id, lhs.is_cdn());
    let rhs_dc = (rhs.raw_dc_id, rhs.is_cdn());
    if lhs_dc != rhs_dc {
        return lhs_dc < rhs_dc;
    }
    if lhs.is_ipv6() != rhs.is_ipv6() {
        return rhs.is_ipv6();
    }
    if lhs.is_media_only() != rhs.is_media_only() {
        return rhs.is_media_only();
    }
    if lhs.is_obfuscated_tcp_only() != rhs.is_obfuscated_tcp_only() {
        return lhs.is_obfuscated_tcp_only();
    }
    if lhs.is_static() != rhs.is_static() {
        return rhs.is_static();
    }
    if lhs.is_ipv6() {
        return false;
    }
    // Hash<int64>()(get_ip_address().get_ipv4() + seed)
    // (HashTableUtils.h:58-60) — `get_ipv4()` is the big-endian packed
    // address, `+ seed` the int64 sum.
    hash_int64(i64::from(parse_ipv4(lhs.ip_address())) + seed)
        < hash_int64(i64::from(parse_ipv4(rhs.ip_address())) + seed)
}

/// `IPAddress::get_ipv4()` — the dotted-quad address packed big-endian
/// (first octet most significant). `0` for anything unparsable: the C++
/// options are address-parsed at construction, but the stored narrowing
/// keeps raw strings until the IPAddress port — they all hash alike
/// instead of being dropped here.
fn parse_ipv4(ip: &str) -> u32 {
    let mut octets = ip.split('.');
    let mut value = 0u32;
    for _ in 0..4 {
        let Some(octet) = octets.next() else {
            return 0;
        };
        match octet.parse::<u32>() {
            Ok(octet) if octet <= 255 => value = (value << 8) | octet,
            _ => return 0,
        }
    }
    if octets.next().is_some() {
        return 0;
    }
    value
}

/// `ConnectionCreator::get_default_dc_options` (cpp:1220-1280) — the
/// built-in endpoints (the non-emscripten table). The ip list shuffle is
/// the C++ `Random::shuffle(vector&)` (secure); `DcOption(DcId, IPAddress)`
/// sets only the `IPv6` flag for the v6 hosts, and the options go
/// port-major (the `for (auto port : ports)` outer loop).
pub(crate) fn get_default_dc_options(is_test: bool) -> Vec<StoredDcOption> {
    fn add_ip_ports(
        res: &mut Vec<StoredDcOption>,
        dc_id: i32,
        ip_addresses: &[&str],
        ports: &[i32],
        ipv6: bool,
    ) {
        let mut ip_addresses = ip_addresses.to_vec();
        tdutils::random::secure_shuffle(&mut ip_addresses);
        for &port in ports {
            for ip_address in &ip_addresses {
                res.push(StoredDcOption {
                    flags: if ipv6 { DC_OPTION_IPV6 } else { 0 },
                    raw_dc_id: dc_id,
                    ip: ip_address.as_bytes().to_vec(),
                    port,
                    secret: Vec::new(),
                });
            }
        }
    }

    let ports = [443, 80, 5222];
    let mut res = Vec::new();
    if is_test {
        add_ip_ports(&mut res, 1, &["149.154.175.10"], &ports, false);
        add_ip_ports(&mut res, 2, &["149.154.167.40"], &ports, false);
        add_ip_ports(&mut res, 3, &["149.154.175.117"], &ports, false);

        add_ip_ports(&mut res, 1, &["2001:b28:f23d:f001::e"], &ports, true);
        add_ip_ports(&mut res, 2, &["2001:67c:4e8:f002::e"], &ports, true);
        add_ip_ports(&mut res, 3, &["2001:b28:f23d:f003::e"], &ports, true);
    } else {
        add_ip_ports(&mut res, 1, &["149.154.175.50"], &ports, false);
        add_ip_ports(
            &mut res,
            2,
            &["149.154.167.51", "95.161.76.100"],
            &ports,
            false,
        );
        add_ip_ports(&mut res, 3, &["149.154.175.100"], &ports, false);
        add_ip_ports(&mut res, 4, &["149.154.167.91"], &ports, false);
        add_ip_ports(&mut res, 5, &["149.154.171.5"], &ports, false);

        add_ip_ports(&mut res, 1, &["2001:b28:f23d:f001::a"], &ports, true);
        add_ip_ports(&mut res, 2, &["2001:67c:4e8:f002::a"], &ports, true);
        add_ip_ports(&mut res, 3, &["2001:b28:f23d:f003::a"], &ports, true);
        add_ip_ports(&mut res, 4, &["2001:67c:4e8:f004::a"], &ports, true);
        add_ip_ports(&mut res, 5, &["2001:b28:f23f:f005::a"], &ports, true);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_manager::{
        DC_OPTION_CDN, DC_OPTION_HAS_SECRET, DC_OPTION_IPV6, DC_OPTION_MEDIA_ONLY,
        DC_OPTION_OBFUSCATED_TCP_ONLY, DC_OPTION_STATIC, StoredDcOptions,
    };
    use tddb::db_key::DbKey;

    /// A `BinlogKeyValue` bound to a temp file, like the recoverer tests.
    struct TestPmc {
        pmc: BinlogKeyValue,
        path: String,
    }

    impl TestPmc {
        fn new(name: &str) -> TestPmc {
            let path = std::env::temp_dir()
                .join(format!(
                    "td-rs-connection-creator-{name}-{}.binlog",
                    std::process::id()
                ))
                .to_string_lossy()
                .into_owned();
            let _ = BinlogKeyValue::destroy(&path);
            let mut pmc = BinlogKeyValue::new();
            pmc.init(&path, DbKey::empty()).ensure();
            TestPmc { pmc, path }
        }
    }

    impl std::ops::Deref for TestPmc {
        type Target = BinlogKeyValue;

        fn deref(&self) -> &BinlogKeyValue {
            &self.pmc
        }
    }

    impl std::ops::DerefMut for TestPmc {
        fn deref_mut(&mut self) -> &mut BinlogKeyValue {
            &mut self.pmc
        }
    }

    impl Drop for TestPmc {
        fn drop(&mut self) {
            self.pmc.close();
            std::fs::remove_file(&self.path).ok();
        }
    }

    fn option(dc_id: i32, ip: &str, port: i32) -> StoredDcOption {
        StoredDcOption {
            flags: 0,
            raw_dc_id: dc_id,
            ip: ip.as_bytes().to_vec(),
            port,
            secret: Vec::new(),
        }
    }

    fn with_flags(mut option: StoredDcOption, flags: i32) -> StoredDcOption {
        option.flags = flags;
        option
    }

    /// Every comparator tier (cpp:1014-1034) in one sweep: dc id first
    /// (the CDN externality second), ipv4 before ipv6, non-media before
    /// media, obfuscated FIRST, non-static before static.
    #[test]
    fn comparator_orders_every_tier() {
        let a = option(2, "10.0.0.1", 443); // plain ipv4
        let c = with_flags(option(2, "10.0.0.3", 443), DC_OPTION_OBFUSCATED_TCP_ONLY);
        let d = with_flags(option(2, "10.0.0.4", 443), DC_OPTION_MEDIA_ONLY);
        let e = with_flags(option(2, "10.0.0.5", 443), DC_OPTION_STATIC);
        let f = with_flags(option(2, "2001:db8::1", 443), DC_OPTION_IPV6);
        let g = with_flags(option(2, "10.0.0.7", 443), DC_OPTION_CDN);
        let b = option(3, "10.0.0.2", 443);

        // dc 2 (internal) < dc 2 (external/CDN) < dc 3.
        assert!(dc_option_less(&a, &b, 0));
        assert!(!dc_option_less(&b, &a, 0));
        assert!(dc_option_less(&a, &g, 0));
        assert!(dc_option_less(&g, &b, 0));

        // Same dc, ipv4: obfuscated c < plain a < static e < media d.
        assert!(dc_option_less(&c, &a, 0));
        assert!(dc_option_less(&a, &e, 0));
        assert!(dc_option_less(&e, &d, 0));

        // All ipv4 before the ipv6 option.
        assert!(dc_option_less(&d, &f, 0));
        assert!(!dc_option_less(&f, &d, 0));

        // The full sort of a shuffled input: the dc tier outranks the ipv6
        // tier, so all internal dc2 options (ipv6 included) precede the
        // CDN one.
        let mut sorted = [
            f.clone(),
            b.clone(),
            d.clone(),
            g.clone(),
            a.clone(),
            e.clone(),
            c.clone(),
        ];
        sorted.sort_by(|lhs, rhs| {
            if dc_option_less(lhs, rhs, 0) {
                Ordering::Less
            } else if dc_option_less(rhs, lhs, 0) {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        });
        let ips: Vec<&str> = sorted
            .iter()
            .map(|option| std::str::from_utf8(&option.ip).unwrap())
            .collect();
        assert_eq!(
            ips,
            [
                "10.0.0.3",
                "10.0.0.1",
                "10.0.0.5",
                "10.0.0.4",
                "2001:db8::1",
                "10.0.0.7",
                "10.0.0.2"
            ]
        );
    }

    /// Two ipv6 options of one dc compare equal at every tier — the
    /// stable sort keeps their input order (the C++ `return false`).
    #[test]
    fn comparator_keeps_ipv6_pairs_stable() {
        let first = with_flags(option(2, "2001:db8::1", 443), DC_OPTION_IPV6);
        let second = with_flags(option(2, "2001:db8::2", 443), DC_OPTION_IPV6);
        assert!(!dc_option_less(&first, &second, 0));
        assert!(!dc_option_less(&second, &first, 0));
    }

    /// The ipv4 tie-break hashes `get_ipv4() + my_id` — the seed is what
    /// spreads clients over the addresses of one DC (cpp:1031-1034).
    #[test]
    fn comparator_tie_breaks_by_the_seeded_hash() {
        let low = option(2, "149.154.167.51", 443);
        let high = option(2, "95.161.76.100", 443);
        // hash_int64(0x959AA733) = 0x3755657e, hash_int64(0x5FA14C64) =
        // 0x1b8df1df (the C++ bodies): the higher address sorts first.
        let expected =
            hash_int64(i64::from(0x959a_a733u32)) < hash_int64(i64::from(0x5fa1_4c64u32));
        assert!(!expected);
        assert_eq!(dc_option_less(&low, &high, 0), expected);
        assert_eq!(dc_option_less(&high, &low, 0), !expected);

        // Some my_id flips the pair: the seed enters the hash input.
        let flips = (1..=1000)
            .any(|seed| dc_option_less(&low, &high, seed) != dc_option_less(&low, &high, 0));
        assert!(flips, "no seed in 1..=1000 flips the pair");
    }

    /// `get_default_dc_options` (cpp:1224-1280): five prod DCs (dc2 with
    /// two addresses) plus the five ipv6 hosts, port-major over
    /// {443, 80, 5222}; the test table has three of each.
    #[test]
    fn default_dc_options_match_the_cpp_table() {
        let prod = get_default_dc_options(false);
        // 6 ipv4 addresses * 3 ports + 5 ipv6 hosts * 3 ports.
        assert_eq!(prod.len(), 33);
        let test = get_default_dc_options(true);
        assert_eq!(test.len(), 18);

        // Port-major: dc1's three options carry the ports in table order.
        let dc1_ports: Vec<i32> = prod[0..3].iter().map(|option| option.port).collect();
        assert_eq!(dc1_ports, [443, 80, 5222]);
        assert_eq!(std::str::from_utf8(&prod[0].ip).unwrap(), "149.154.175.50");
        assert_eq!(prod[0].flags, 0);

        // dc2 spreads over both addresses at every port (the secure
        // shuffle only swaps them).
        for port in [443, 80, 5222] {
            let mut dc2: Vec<&str> = prod
                .iter()
                .filter(|option| option.raw_dc_id == 2 && option.port == port && option.flags == 0)
                .map(|option| std::str::from_utf8(&option.ip).unwrap())
                .collect();
            dc2.sort_unstable();
            assert_eq!(dc2, ["149.154.167.51", "95.161.76.100"]);
        }

        // The ipv6 hosts carry only the IPv6 flag, after the ipv4 table.
        let ipv6_start = prod.len() - 15;
        assert_eq!(
            std::str::from_utf8(&prod[ipv6_start].ip).unwrap(),
            "2001:b28:f23d:f001::a"
        );
        assert_eq!(prod[ipv6_start].flags, DC_OPTION_IPV6);
        assert_eq!(prod[ipv6_start].raw_dc_id, 1);

        // The test table's first entry.
        assert_eq!(std::str::from_utf8(&test[0].ip).unwrap(), "149.154.175.10");
    }

    /// `on_dc_options` (cpp:1012-1037): the sorted update persists to the
    /// `"dc_options"` binlog key, and the set rebuilds from the defaults
    /// (a second call replaces, not appends).
    #[test]
    fn on_dc_options_sorts_persists_and_merges() {
        let mut pmc = TestPmc::new("on-dc-options");
        let mut creator = ConnectionCreator::new(&mut pmc, false);
        let defaults_len = creator.dc_options().len();

        let update = vec![
            option(2, "95.161.76.100", 443),
            option(2, "149.154.167.51", 443),
            option(1, "149.154.175.50", 80),
            with_flags(
                option(1, "149.154.175.50", 443),
                DC_OPTION_OBFUSCATED_TCP_ONLY,
            ),
        ];
        creator.on_dc_options(update, 0, false, &mut pmc);

        // The persisted blob is exactly the sorted update (the defaults
        // never persist): the obfuscated dc1 443 precedes its plain 80,
        // then dc2 by the seeded hash (0x1b8df1df < 0x3755657e —
        // 95.161.76.100 first).
        let stored = StoredDcOptions::parse(&pmc.get_raw(b"dc_options")).unwrap();
        let stored_ips: Vec<&str> = stored
            .options
            .iter()
            .map(|option| std::str::from_utf8(&option.ip).unwrap())
            .collect();
        assert_eq!(
            stored_ips,
            [
                "149.154.175.50",
                "149.154.175.50",
                "95.161.76.100",
                "149.154.167.51"
            ]
        );
        assert_eq!(stored.options[0].port, 443); // the obfuscated 443 first
        assert_eq!(stored.options[0].flags, DC_OPTION_OBFUSCATED_TCP_ONLY);

        // The set: defaults first, then the sorted update.
        assert_eq!(creator.dc_options().len(), defaults_len + 4);
        assert_eq!(
            std::str::from_utf8(&creator.dc_options()[defaults_len].ip).unwrap(),
            "149.154.175.50"
        );

        // A second call replaces the previous update (reset + defaults).
        creator.on_dc_options(vec![option(4, "149.154.167.91", 443)], 0, false, &mut pmc);
        assert_eq!(creator.dc_options().len(), defaults_len + 1);
        let stored = StoredDcOptions::parse(&pmc.get_raw(b"dc_options")).unwrap();
        assert_eq!(stored.options.len(), 1);
        assert_eq!(stored.options[0].raw_dc_id, 4);
    }

    /// An empty update persists the empty list and leaves the set at the
    /// defaults — the `start_up` failure path calls exactly this
    /// (cpp:1102).
    #[test]
    fn an_empty_update_persists_empty_and_keeps_defaults() {
        let mut pmc = TestPmc::new("empty-update");
        let mut creator = ConnectionCreator::new(&mut pmc, false);
        let defaults_len = creator.dc_options().len();

        creator.on_dc_options(Vec::new(), 0, false, &mut pmc);
        let stored = StoredDcOptions::parse(&pmc.get_raw(b"dc_options")).unwrap();
        assert!(stored.options.is_empty());
        assert_eq!(creator.dc_options().len(), defaults_len);
    }

    /// The `start_up` replay (cpp:1099-1106): a parsable store rebuilds
    /// defaults + the stored options in their stored (already sorted)
    /// order without re-sorting or re-persisting; an unparsable store
    /// routes through the empty update.
    #[test]
    fn start_up_replays_the_stored_options() {
        let mut pmc = TestPmc::new("start-up");

        // Store an anti-sorted pair; the replay must keep it verbatim.
        let stored = StoredDcOptions {
            options: vec![option(3, "10.0.0.3", 443), option(1, "10.0.0.1", 443)],
        };
        pmc.set_raw(b"dc_options", &stored.store());
        let creator = ConnectionCreator::new(&mut pmc, false);
        let options = creator.dc_options();
        let defaults_len = options.len() - 2;
        assert_eq!(
            std::str::from_utf8(&options[defaults_len].ip).unwrap(),
            "10.0.0.3"
        );
        assert_eq!(
            std::str::from_utf8(&options[defaults_len + 1].ip).unwrap(),
            "10.0.0.1"
        );
        // No re-persist: the blob still holds the stored order.
        let replayed = StoredDcOptions::parse(&pmc.get_raw(b"dc_options")).unwrap();
        assert_eq!(replayed.options, stored.options);

        // A corrupt store: the empty update runs — the blob becomes the
        // empty list, the set is defaults-only.
        let mut pmc = TestPmc::new("start-up-corrupt");
        pmc.set_raw(b"dc_options", &[1, 2, 3]); // too short for a vector
        let creator = ConnectionCreator::new(&mut pmc, false);
        assert!(
            StoredDcOptions::parse(&pmc.get_raw(b"dc_options"))
                .unwrap()
                .options
                .is_empty()
        );
        assert_eq!(
            creator.dc_options().len(),
            get_default_dc_options(false).len()
        );
    }

    /// The stored flags survive the persist round-trip — the secret is
    /// only written with its flag (DcOptions.h:143-152).
    #[test]
    fn secrets_round_trip_through_the_store() {
        let mut pmc = TestPmc::new("secret");
        let mut creator = ConnectionCreator::new(&mut pmc, false);
        creator.on_dc_options(
            vec![StoredDcOption {
                flags: DC_OPTION_OBFUSCATED_TCP_ONLY | DC_OPTION_HAS_SECRET,
                raw_dc_id: 2,
                ip: b"149.154.167.51".to_vec(),
                port: 443,
                secret: b"16-byte--secret".to_vec(),
            }],
            0,
            false,
            &mut pmc,
        );
        let stored = StoredDcOptions::parse(&pmc.get_raw(b"dc_options")).unwrap();
        assert_eq!(stored.options[0].secret(), b"16-byte--secret");
        assert!(stored.options[0].is_obfuscated_tcp_only());
    }

    /// The proxy env over a real `BinlogKeyValue` — the option writes
    /// ride a map, the promo/state/mtproto seams count their calls, and
    /// the net seams drop the promises (the production `ConnectionEnv`
    /// behavior).
    struct TestProxyEnv {
        pmc: TestPmc,
        options: std::collections::BTreeMap<String, String>,
        promo_reloads: usize,
        sponsored_removals: usize,
        proxy_states: Vec<bool>,
        mtproto_updates: Vec<Option<ProxyType>>,
        /// the recorded `set_active_proxy` fan-outs (#125)
        web_proxy_calls: Vec<(i32, Proxy)>,
    }

    impl TestProxyEnv {
        fn new(name: &str) -> TestProxyEnv {
            TestProxyEnv {
                pmc: TestPmc::new(name),
                options: std::collections::BTreeMap::new(),
                promo_reloads: 0,
                sponsored_removals: 0,
                proxy_states: Vec::new(),
                mtproto_updates: Vec::new(),
                web_proxy_calls: Vec::new(),
            }
        }

        fn proxy(server: &str, port: i32) -> td_api::Proxy {
            td_api::Proxy {
                server: server.to_string(),
                port,
                r#type: Some(td_api::ProxyType::proxyTypeSocks5(
                    td_api::proxyTypeSocks5 {
                        username: "user".to_string(),
                        password: "pass".to_string(),
                    },
                )),
            }
        }
    }

    impl ConnectionCreatorEnv for TestProxyEnv {
        fn binlog_pmc_get(&mut self, key: &[u8]) -> String {
            self.pmc.get(&String::from_utf8_lossy(key))
        }

        fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
            self.pmc.set_raw(key, value);
        }

        fn binlog_pmc_erase(&mut self, key: &str) {
            self.pmc.erase(key);
        }

        fn binlog_pmc_prefix_get(&mut self, prefix: &str) -> Vec<(String, Vec<u8>)> {
            self.pmc.prefix_get(prefix)
        }

        fn get_option_boolean(&self, name: &str, default_value: bool) -> bool {
            match self.options.get(name).map(String::as_str) {
                None | Some("") => default_value,
                Some("Btrue") => true,
                Some("Bfalse") => false,
                Some(value) => panic!("non-boolean option {name} = {value}"),
            }
        }

        fn set_option_integer(&mut self, name: &str, value: i64) {
            self.options.insert(name.to_string(), format!("I{value}"));
        }

        fn set_option_empty(&mut self, name: &str) {
            self.options.insert(name.to_string(), String::new());
        }

        fn request_config(&mut self, _reopen_sessions: bool) {}

        fn remove_sponsored_dialog(&mut self) {
            self.sponsored_removals += 1;
        }

        fn reload_promo_data(&mut self) {
            self.promo_reloads += 1;
        }

        fn update_mtproto_header(&mut self, proxy: Option<&Proxy>) {
            self.mtproto_updates.push(proxy.map(|proxy| proxy.type_));
        }

        fn on_proxy_state(&mut self, use_proxy: bool) {
            self.proxy_states.push(use_proxy);
        }

        fn ping_via_net(&mut self, _proxy: &Proxy, promise: Promise<f64>) {
            drop(promise);
        }

        fn test_proxy_via_net(
            &mut self,
            _proxy: &Proxy,
            _dc_id: i32,
            _timeout: f64,
            promise: Promise<()>,
        ) {
            drop(promise);
        }

        fn web_proxy_set_active_proxy(&mut self, proxy_id: i32, proxy: Proxy) {
            self.web_proxy_calls.push((proxy_id, proxy));
        }
    }

    fn socks5(server: &str, port: i32) -> Proxy {
        Proxy {
            type_: ProxyType::Socks5,
            server: server.to_string(),
            port,
            user: "user".to_string(),
            password: "pass".to_string(),
            path: String::new(),
            secret: ProxySecret::default(),
        }
    }

    /// The 1.8.70 web-proxy ladder (net/Proxy.cpp:159-173) — the port
    /// gate rides before the server/port validation, the host
    /// normalization answers the C++ reference forms (upstream
    /// test/link.cpp vectors), and the store/parse bytes keep the
    /// tl_helpers field order (b54698832).
    #[test]
    fn web_proxies_create_store_and_round_trip_like_the_cpp() {
        let web_type = |path: &str, secret: &str| {
            td_api::ProxyType::proxyTypeWeb(td_api::proxyTypeWeb {
                path: path.to_owned(),
                secret: secret.to_owned(),
            })
        };

        // the port gate: 0 and 443 pass, everything else is the 400 band
        assert_eq!(
            Proxy::create_proxy_parts(
                "proxy.example.com",
                8080,
                Some(&web_type("", "8561944064fc730cbfa4473562d8ec59"))
            )
            .unwrap_err(),
            "Web proxy port must be 443"
        );

        // the plain hex secret form, port 0 → forced 443
        let proxy = Proxy::create_proxy_parts(
            "proxy.example.com",
            0,
            Some(&web_type("", "8561944064fc730cbfa4473562d8ec59")),
        )
        .expect("a plain web proxy");
        assert_eq!(proxy.type_, ProxyType::Web);
        assert_eq!(proxy.port, Proxy::WEB_PROXY_PORT);
        assert_eq!(proxy.server, "proxy.example.com");
        assert_eq!(proxy.path, "");

        // host normalization: scheme/trailing-dot strip, the punycode
        // pass, and the two rejection bands — the generic invalid-host
        // ladder and the distinct WHATWG "ends in a number" error
        let normalized = |server: &str| {
            Proxy::create_proxy_parts(
                server,
                443,
                Some(&web_type("", "8561944064fc730cbfa4473562d8ec59")),
            )
            .map(|proxy| proxy.server)
        };
        assert_eq!(
            normalized("HTTPS://Proxy.Example.COM.").as_deref(),
            Ok("proxy.example.com")
        );
        assert_eq!(
            normalized("Пример.РФ").as_deref(),
            Ok("xn--e1afmkfd.xn--p1ai")
        );
        for server in ["localhost", "", "a", "-a.b", "a-.b"] {
            assert_eq!(
                normalized(server).unwrap_err(),
                "Invalid web proxy server",
                "host must be invalid: {server}"
            );
        }
        for server in ["1.2.3.4", "proxy.0x7f", "proxy.127.1"] {
            assert_eq!(
                normalized(server).unwrap_err(),
                "Web proxy server must be a domain name",
                "host must be a number-ending rejection: {server}"
            );
        }

        // the path validator: segment starts are alphanumeric, separators
        // are '-'/'_', '/' only between segments
        let with_path = |path: &str| {
            Proxy::create_proxy_parts(
                "proxy.example.com",
                443,
                Some(&web_type(path, "dd8561944064fc730cbfa4473562d8ec59")),
            )
            .map(|proxy| proxy.path)
        };
        assert_eq!(with_path("a/b_c").as_deref(), Ok("a/b_c"));
        assert_eq!(
            with_path("-app").unwrap_err(),
            "Invalid web proxy path",
            "a segment must not start with a separator"
        );
        assert_eq!(
            with_path("a//b").unwrap_err(),
            "Invalid web proxy path",
            "an empty segment is the C++ is_segment_start band"
        );
        assert_eq!(
            with_path("a.b").unwrap_err(),
            "Invalid web proxy path",
            "a dot is not a path byte"
        );
        assert_eq!(with_path("").as_deref(), Ok(""), "an empty path is valid");

        // fake-TLS secrets are refused for web proxies (the relay never
        // answers the TLS handshake) — the vector is the upstream
        // ee-prefixed secret of test/link.cpp's regular-proxy band
        assert_eq!(
            Proxy::get_web_proxy_secret("7tAAAAAAAAAAAAAAAAAAAAAAAAcuZ29vZ2xlLmNvbQ").unwrap_err(),
            "Web proxies don't support fake TLS secrets"
        );

        // the 0x70 marked link-secret round trip: a path-carrying link
        // secret decodes to the dd-padded secret; the same secret without
        // a path falls back to the plain form
        let marked = Proxy::get_web_proxy_link_secret("cN2FYZRAZPxzDL-kRzVi2OxZ", true).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&marked.get_encoded_secret()),
            "dd8561944064fc730cbfa4473562d8ec59"
        );
        let unmarked = Proxy::get_web_proxy_link_secret("8561944064fc730cbfa4473562d8ec59", false);
        assert!(unmarked.is_ok(), "the plain hex form parses without a path");
        assert_eq!(
            Proxy::get_web_proxy_link_secret("8561944064fc730cbfa4473562d8ec59", true).unwrap_err(),
            "Wrong proxy secret",
            "an unmarked secret is rejected when the proxy has a path"
        );

        // parse_web_proxy_address: the "host[/path]" address form
        assert_eq!(
            Proxy::parse_web_proxy_address(" proxy.example.com/a/b_c ").unwrap(),
            ("proxy.example.com".to_owned(), "a/b_c".to_owned())
        );

        // the store bytes: the Web type int32 leads, then server, port,
        // encoded secret, path — and parse round-trips (the C++
        // tl_helpers order, Proxy.h:186-191)
        let pathed = Proxy::create_proxy_parts(
            "proxy.example.com",
            443,
            Some(&web_type("a/b_c", "dd8561944064fc730cbfa4473562d8ec59")),
        )
        .unwrap();
        let mut round_tripped = Proxy::default();
        round_tripped.parse(&pathed.store()).unwrap();
        assert_eq!(round_tripped, pathed);
        // the type discriminant is 5 in the stored form
        assert_eq!(&pathed.store()[..4], &[5, 0, 0, 0]);

        // the td_api object shape (Proxy.cpp:224-226): the path and the
        // encoded secret rebuild proxyTypeWeb
        let object = pathed.get_proxy_object();
        assert_eq!(object.port, 443);
        match object.r#type.expect("the proxy type") {
            td_api::ProxyType::proxyTypeWeb(type_web) => {
                assert_eq!(type_web.path, "a/b_c");
                assert_eq!(type_web.secret, "dd8561944064fc730cbfa4473562d8ec59");
            }
            other => panic!("unexpected proxy type {other:?}"),
        }

        // the mtproto-transport condition (net/Proxy.h:145-147): the web
        // variant counts like Mtproto for the header update; the socks
        // variant does not
        assert!(pathed.use_mtproto_transport_proxy());
        assert!(!socks5("s", 80).use_mtproto_transport_proxy());
    }

    /// The web variant through the existing proxy functions — `add_proxy`
    /// (ConnectionCreator.cpp:117-160) accepts it like any other type,
    /// routes the enable through the mtproto-header hop the C++
    /// `use_mtproto_transport_proxy()` condition fires (cpp:429-431),
    /// answers it in `get_proxies`, and the pmc record round-trips the
    /// registry through a reopen.
    #[test]
    fn web_proxies_flow_through_the_existing_proxy_functions() {
        let mut env = TestProxyEnv::new("web-proxy-functions");
        let mut creator = ConnectionCreator::default();
        let web = td_api::Proxy {
            server: "Proxy.Example.COM.".to_owned(),
            port: 0,
            r#type: Some(td_api::ProxyType::proxyTypeWeb(td_api::proxyTypeWeb {
                path: "a/b_c".to_owned(),
                secret: "dd8561944064fc730cbfa4473562d8ec59".to_owned(),
            })),
        };

        // add+enable: id 2, the header hop fires for the web proxy (the
        // socks5 add never fired one), and the enabled option lands.
        let added = creator
            .add_proxy(&mut env, -1, Some(&web), true, "relay".to_owned())
            .unwrap();
        assert_eq!(added.id, 2);
        assert!(added.is_enabled);
        assert_eq!(env.mtproto_updates, vec![Some(ProxyType::Web)]);
        assert_eq!(
            env.options.get("enabled_proxy_id").map(String::as_str),
            Some("I2")
        );

        // get_proxies answers the normalized proxyTypeWeb object.
        let proxies = creator.get_proxies();
        let listed = &proxies.proxies[0];
        assert_eq!(listed.id, 2);
        assert!(listed.is_enabled);
        let listed_proxy = listed.proxy.as_ref().expect("the proxy object");
        assert_eq!(listed_proxy.server, "proxy.example.com");
        assert_eq!(listed_proxy.port, 443);
        match listed_proxy.r#type.as_ref().expect("the proxy type") {
            td_api::ProxyType::proxyTypeWeb(type_web) => {
                assert_eq!(type_web.path, "a/b_c");
                assert_eq!(type_web.secret, "dd8561944064fc730cbfa4473562d8ec59");
            }
            other => panic!("unexpected proxy type {other:?}"),
        }

        // the header clears on the disable hop (cpp:448-450), like an
        // mtproto proxy
        creator.disable_proxy(&mut env);
        assert_eq!(env.mtproto_updates, vec![Some(ProxyType::Web), None]);

        // the pmc record round-trips a reopen: restore_proxies rebuilds
        // the registry with the web variant intact.
        drop(creator);
        let mut restored = ConnectionCreator::default();
        restored.restore_proxies(&mut env);
        let proxies = restored.get_proxies();
        assert_eq!(proxies.proxies.len(), 1);
        assert_eq!(restored.proxies[&2].0.type_, ProxyType::Web);
        assert_eq!(restored.proxies[&2].0.path, "a/b_c");
    }

    /// The `create_proxy` 400 ladder (Proxy.cpp:12-48), each band in
    /// order, plus the http_only split and the mtproto secret parse.
    #[test]
    fn create_proxy_ladder_matches_the_cpp_errors() {
        let proxy = TestProxyEnv::proxy("proxy.example", 1080);
        let typed = |r#type| td_api::Proxy {
            r#type: Some(r#type),
            ..TestProxyEnv::proxy("proxy.example", 1080)
        };

        assert_eq!(
            Proxy::create_proxy(None).unwrap_err(),
            "Proxy must be non-empty"
        );
        assert_eq!(
            Proxy::create_proxy_parts("", 1080, proxy.r#type.as_ref()).unwrap_err(),
            "Server name must be non-empty"
        );
        let long_server = "x".repeat(256);
        assert_eq!(
            Proxy::create_proxy_parts(&long_server, 1080, proxy.r#type.as_ref()).unwrap_err(),
            "Server name is too long"
        );
        // The utf8 band (Proxy.cpp:26-28) is dead code here — a Rust
        // `&str` cannot carry the invalid bytes it checks for; the
        // C++-reachable callers (testProxy skips the clean) keep it.
        assert_eq!(
            Proxy::create_proxy_parts("s", 0, proxy.r#type.as_ref()).unwrap_err(),
            "Wrong port number"
        );
        assert_eq!(
            Proxy::create_proxy_parts("s", 65536, proxy.r#type.as_ref()).unwrap_err(),
            "Wrong port number"
        );
        assert_eq!(
            Proxy::create_proxy_parts("s", 1080, None).unwrap_err(),
            "Proxy type must be non-empty"
        );

        let created = Proxy::create_proxy(Some(&proxy)).unwrap();
        assert_eq!(created, socks5("proxy.example", 1080));

        let http = typed(td_api::ProxyType::proxyTypeHttp(td_api::proxyTypeHttp {
            username: "user".to_string(),
            password: "pass".to_string(),
            http_only: false,
        }));
        assert_eq!(
            Proxy::create_proxy(Some(&http)).unwrap().type_,
            ProxyType::HttpTcp
        );
        let caching = typed(td_api::ProxyType::proxyTypeHttp(td_api::proxyTypeHttp {
            username: "user".to_string(),
            password: "pass".to_string(),
            http_only: true,
        }));
        assert_eq!(
            Proxy::create_proxy(Some(&caching)).unwrap().type_,
            ProxyType::HttpCaching
        );

        let mtproto = typed(td_api::ProxyType::proxyTypeMtproto(
            td_api::proxyTypeMtproto {
                secret: "00112233445566778899aabbccddeeff".to_string(),
            },
        ));
        let created = Proxy::create_proxy(Some(&mtproto)).unwrap();
        assert_eq!(created.type_, ProxyType::Mtproto);
        assert_eq!(
            created.secret.get_encoded_secret(),
            b"00112233445566778899aabbccddeeff".to_vec()
        );
        let bad_secret = typed(td_api::ProxyType::proxyTypeMtproto(
            td_api::proxyTypeMtproto {
                secret: "zz-not-a-secret".to_string(),
            },
        ));
        assert!(Proxy::create_proxy(Some(&bad_secret)).is_err());
    }

    /// The tl_helpers store bytes pinned by hand: the `Type` int32, then
    /// the socks5 field group (`store(string)` = int32 length + bytes).
    #[test]
    fn proxy_store_parse_round_trip_is_byte_exact() {
        let proxy = socks5("s", 1080);
        assert_eq!(
            proxy.store(),
            vec![
                1, 0, 0, 0, // Type::Socks5
                1, b's', 0, 0, // "s"
                56, 4, 0, 0, // 1080
                4, b'u', b's', b'e', b'r', 0, 0, 0, // "user"
                4, b'p', b'a', b's', b's', 0, 0, 0, // "pass"
            ]
        );
        let mut parsed = Proxy::default();
        parsed.parse(&proxy.store()).unwrap();
        assert_eq!(parsed, proxy);

        let mtproto = Proxy {
            type_: ProxyType::Mtproto,
            server: "m".to_string(),
            port: 443,
            user: String::new(),
            password: String::new(),
            path: String::new(),
            secret: ProxySecret::from_link(b"00112233445566778899aabbccddeeff", false).unwrap(),
        };
        assert_eq!(
            mtproto.store(),
            vec![
                2, 0, 0, 0, // Type::Mtproto
                1, b'm', 0, 0, // "m"
                187, 1, 0, 0, // 443
                32, b'0', b'0', b'1', b'1', b'2', b'2', b'3', b'3', b'4', b'4', b'5', b'5', b'6',
                b'6', b'7', b'7', b'8', b'8', b'9', b'9', b'a', b'a', b'b', b'b', b'c', b'c', b'd',
                b'd', b'e', b'e', b'f', b'f', 0, 0, 0, // the encoded secret
            ]
        );
        let mut parsed = Proxy::default();
        parsed.parse(&mtproto.store()).unwrap();
        assert_eq!(parsed, mtproto);
    }

    /// The `add_proxy` decision table: the allocate/dedup/edit-same/
    /// edit-different walks, the error bands, and the enable fan-outs.
    #[test]
    fn add_proxy_registry_decisions_match_the_cpp() {
        let mut env = TestProxyEnv::new("add-proxy-decisions");
        let mut creator = ConnectionCreator::default();

        // A fresh add allocates id 2 — id 1 stays reserved for the legacy
        // bare "proxy" record — stores it under "proxy2", answers disabled.
        let added = creator
            .add_proxy(
                &mut env,
                -1,
                Some(&TestProxyEnv::proxy("a", 1080)),
                false,
                String::new(),
            )
            .unwrap();
        assert_eq!(added.id, 2);
        assert!(!added.is_enabled);
        assert_eq!(added.last_used_date, 0);
        assert!(!env.pmc.get("proxy2").is_empty());
        assert!(!env.options.contains_key("enabled_proxy_id"));

        // Re-adding the same proxy dedups onto id 2; the comment updates.
        let deduped = creator
            .add_proxy(
                &mut env,
                -1,
                Some(&TestProxyEnv::proxy("a", 1080)),
                false,
                "main".to_string(),
            )
            .unwrap();
        assert_eq!(deduped.id, 2);
        assert_eq!(deduped.comment, "main");
        assert_eq!(env.pmc.get("proxy_comment2"), "main");

        // The second proxy gets id 3 and pins proxy_max_id past it.
        let second = creator
            .add_proxy(
                &mut env,
                -1,
                Some(&TestProxyEnv::proxy("b", 1080)),
                false,
                String::new(),
            )
            .unwrap();
        assert_eq!(second.id, 3);
        assert_eq!(env.pmc.get("proxy_max_id"), "4");

        // The error bands.
        assert_eq!(
            creator
                .add_proxy(
                    &mut env,
                    7,
                    Some(&TestProxyEnv::proxy("a", 1080)),
                    false,
                    String::new()
                )
                .unwrap_err(),
            "Proxy not found"
        );
        assert_eq!(
            creator.enable_proxy(&mut env, 9).unwrap_err(),
            "Unknown proxy identifier"
        );
        assert_eq!(
            creator.remove_proxy(&mut env, 9).unwrap_err(),
            "Unknown proxy identifier"
        );

        // Enabling proxy 3: the option, the state flag, no mtproto header
        // hop for socks5.
        creator.enable_proxy(&mut env, 3).unwrap();
        assert_eq!(
            env.options.get("enabled_proxy_id").map(String::as_str),
            Some("I3")
        );
        assert_eq!(env.proxy_states, vec![true]);
        // The `set_active_proxy` fan-out (cpp:463-464) — the enabled id
        // with its proxy rides to the web-proxy manager.
        assert_eq!(
            env.web_proxy_calls
                .last()
                .map(|(id, proxy)| (*id, proxy.server.clone())),
            Some((3, "b".to_string()))
        );
        assert!(env.mtproto_updates.is_empty());
        assert!(creator.get_proxies().proxies[1].is_enabled);

        // Editing the active proxy onto a different server keeps it
        // enabled (the C++ `enable = true` mutation) and reuses the id.
        let edited = creator
            .add_proxy(
                &mut env,
                3,
                Some(&TestProxyEnv::proxy("c", 1080)),
                false,
                String::new(),
            )
            .unwrap();
        assert_eq!(edited.id, 3);
        assert!(edited.is_enabled);
        assert_eq!(env.pmc.get("proxy_comment2"), "main");
        assert_eq!(env.pmc.get("proxy_comment3"), "");

        // Disabling: the option clears, the sponsored dialogs drop, the
        // promo reloads.
        creator.disable_proxy(&mut env);
        assert_eq!(
            env.options.get("enabled_proxy_id").map(String::as_str),
            Some("")
        );
        assert!(env.sponsored_removals >= 1);
        assert!(env.promo_reloads >= 1);
        // The disable fan-out carries the cleared pair (cpp:463-464).
        assert_eq!(env.web_proxy_calls.last().map(|(id, _)| *id), Some(0));

        // Removing the active-proxy record erases the pmc keys.
        creator.enable_proxy(&mut env, 2).unwrap();
        creator.remove_proxy(&mut env, 2).unwrap();
        assert!(env.pmc.get("proxy2").is_empty());
        assert!(env.pmc.get("proxy_comment2").is_empty());
        assert!(env.pmc.get("proxy_used2").is_empty());
        assert_eq!(creator.get_proxies().proxies.len(), 1);
    }

    /// The `init_proxies` restore (cpp:1131-1190): registry, comments,
    /// active id and the never-added gate, over a reopen of the same pmc.
    #[test]
    fn restore_proxies_round_trips_through_the_pmc() {
        let mut env = TestProxyEnv::new("restore-proxies");
        let mut creator = ConnectionCreator::default();
        creator
            .add_proxy(
                &mut env,
                -1,
                Some(&TestProxyEnv::proxy("a", 1080)),
                true,
                "note".to_string(),
            )
            .unwrap();
        creator
            .add_proxy(
                &mut env,
                -1,
                Some(&TestProxyEnv::proxy("b", 8080)),
                false,
                String::new(),
            )
            .unwrap();
        drop(creator);

        // A fresh creator restores both records, the comment and the
        // active id (and re-applies the option through the env).
        let mut restored = ConnectionCreator::default();
        restored.restore_proxies(&mut env);
        let proxies = restored.get_proxies();
        assert_eq!(proxies.proxies.len(), 2);
        assert_eq!(proxies.proxies[0].proxy.as_ref().unwrap().server, "a");
        assert_eq!(proxies.proxies[0].comment, "note");
        assert!(proxies.proxies[0].is_enabled);
        assert!(!proxies.proxies[1].is_enabled);
        assert_eq!(
            env.options.get("enabled_proxy_id").map(String::as_str),
            Some("I2")
        );

        // The next allocation continues past the restored max.
        let third = restored
            .add_proxy(
                &mut env,
                -1,
                Some(&TestProxyEnv::proxy("c", 1080)),
                false,
                String::new(),
            )
            .unwrap();
        assert_eq!(third.id, 4);

        // The never-added gate: an empty store pins max_proxy_id = 2 and
        // allocates from 1.
        let mut fresh = TestProxyEnv::new("restore-proxies-fresh");
        let mut fresh_creator = ConnectionCreator::default();
        fresh_creator.restore_proxies(&mut fresh);
        let added = fresh_creator
            .add_proxy(
                &mut fresh,
                -1,
                Some(&TestProxyEnv::proxy("x", 1080)),
                false,
                String::new(),
            )
            .unwrap();
        assert_eq!(added.id, 2);
    }

    /// The `NetType` mapping table (net/NetType.h:19-39).
    #[test]
    fn net_type_mapping_matches_the_cpp() {
        let cases = [
            (
                &td_api::NetworkType::networkTypeOther(td_api::networkTypeOther {}),
                NetType::Other,
            ),
            (
                &td_api::NetworkType::networkTypeWiFi(td_api::networkTypeWiFi {}),
                NetType::WiFi,
            ),
            (
                &td_api::NetworkType::networkTypeMobile(td_api::networkTypeMobile {}),
                NetType::Mobile,
            ),
            (
                &td_api::NetworkType::networkTypeMobileRoaming(td_api::networkTypeMobileRoaming {}),
                NetType::MobileRoaming,
            ),
            (
                &td_api::NetworkType::networkTypeNone(td_api::networkTypeNone {}),
                NetType::None,
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(get_net_type(Some(input)), expected);
        }
        assert_eq!(get_net_type(None), NetType::Other);
    }
}
