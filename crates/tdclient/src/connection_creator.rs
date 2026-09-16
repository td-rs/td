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

use std::cmp::Ordering;

use tddb::binlog_key_value::BinlogKeyValue;
use tdutils::hash::hash_int64;

use crate::config_manager::{DC_OPTION_IPV6, StoredDcOption, StoredDcOptions};

/// The merged option list — the `dc_options_set_` stand-in (see the
/// module docs).
#[derive(Debug, Default)]
pub(crate) struct ConnectionCreator {
    dc_options: Vec<StoredDcOption>,
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
}
