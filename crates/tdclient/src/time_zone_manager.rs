//! Port of `td/telegram/TimeZoneManager.{h,cpp}` (Track I, TimeZones M.1):
//! the `TimeZone`/`TimeZoneList` data structures with their `td::store`
//! binlog-pmc persistence (`"time_zones"` key) and the td_api object
//! mapping.
//!
//! The `help.getTimezonesList` query path (`reload_time_zones`) lands with
//! the net step — until then only the pmc-backed list is served, exactly
//! the state a first run reaches before its first successful reload.

use crate::global::Global;
use crate::option_manager::OptionManager;
use crate::td_api;

/// `TimeZoneManager::TimeZone` — stored as
/// `store(id) + store(name) + store(utc_offset)` (TimeZoneManager.cpp:60-76;
/// no flags word yet — BEGIN/END_STORE_FLAGS is empty).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TimeZone {
    pub id: String,
    pub name: String,
    pub utc_offset: i32,
}

impl TimeZone {
    /// `TimeZone::get_time_zone_object`.
    pub fn get_time_zone_object(&self) -> td_api::TimeZone {
        td_api::TimeZone {
            id: self.id.clone(),
            name: self.name.clone(),
            utc_time_offset: self.utc_offset,
        }
    }

    fn store(&self, out: &mut Vec<u8>) {
        store_string(out, self.id.as_bytes());
        store_string(out, self.name.as_bytes());
        out.extend_from_slice(&self.utc_offset.to_le_bytes());
    }

    fn parse(input: &mut &[u8]) -> Result<TimeZone, String> {
        let id = parse_string(input)?;
        let name = parse_string(input)?;
        let utc_offset = parse_i32(input)?;
        Ok(TimeZone {
            id,
            name,
            utc_offset,
        })
    }
}

/// `TimeZoneManager::TimeZoneList` — stored as
/// `store(vector<TimeZone>) + store(hash)`; `is_loaded_` is runtime state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TimeZoneList {
    pub time_zones: Vec<TimeZone>,
    pub hash: i32,
    pub is_loaded: bool,
}

impl TimeZoneList {
    /// `TimeZoneList::get_time_zones_object`.
    pub fn get_time_zones_object(&self) -> td_api::TimeZones {
        td_api::TimeZones {
            time_zones: self
                .time_zones
                .iter()
                .map(|time_zone| time_zone.get_time_zone_object())
                .collect(),
        }
    }

    fn store(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.time_zones.len() as i32).to_le_bytes());
        for time_zone in &self.time_zones {
            time_zone.store(out);
        }
        out.extend_from_slice(&self.hash.to_le_bytes());
    }

    fn parse(input: &mut &[u8]) -> Result<TimeZoneList, String> {
        let count = parse_i32(input)?;
        if count < 0 {
            return Err("negative vector size".to_string());
        }
        let mut time_zones = Vec::with_capacity(count as usize);
        for _ in 0..count {
            time_zones.push(TimeZone::parse(input)?);
        }
        let hash = parse_i32(input)?;
        Ok(TimeZoneList {
            time_zones,
            hash,
            is_loaded: true,
        })
    }
}

/// `td::store(string)` — the same TL string layout the binlog pmc uses for
/// keys and values (`[len: i32][bytes][zero padding to 4]`).
fn store_string(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as i32).to_le_bytes());
    out.extend_from_slice(s);
    let pad = (4 - s.len() % 4) % 4;
    out.extend(std::iter::repeat_n(0u8, pad));
}

fn parse_string(input: &mut &[u8]) -> Result<String, String> {
    if input.len() < 4 {
        return Err("string length is out of range".to_string());
    }
    let len = i32::from_le_bytes(input[0..4].try_into().unwrap());
    if len < 0 {
        return Err("negative string length".to_string());
    }
    let len = len as usize;
    let padded = (len + 3) & !3;
    if input.len() < 4 + padded {
        return Err("string data is out of range".to_string());
    }
    let bytes = &input[4..4 + len];
    *input = &input[4 + padded..];
    String::from_utf8(bytes.to_vec()).map_err(|_| "string is not UTF-8".to_string())
}

fn parse_i32(input: &mut &[u8]) -> Result<i32, String> {
    if input.len() < 4 {
        return Err("int32 data is out of range".to_string());
    }
    let value = i32::from_le_bytes(input[0..4].try_into().unwrap());
    *input = &input[4..];
    Ok(value)
}

/// `td::TimeZoneManager` — the M.1 subset (the actor wrapper and the
/// `help.getTimezonesList` query arrive with the net step).
#[derive(Default)]
pub struct TimeZoneManager {
    time_zones: TimeZoneList,
}

impl TimeZoneManager {
    /// `TimeZoneManager::get_time_zones_database_key`.
    pub fn database_key() -> &'static str {
        "time_zones"
    }

    /// `TimeZoneManager::load_time_zones` — the pmc-backed lazy load; sets
    /// `is_loaded` even on an empty store, and resets to a default list on
    /// a parse failure (including the loaded flag — the next call retries),
    /// like the C++. Read through `get_raw`: the blob is binary and must
    /// survive byte-exact.
    pub fn load_time_zones(&mut self, global: &mut Global) {
        if self.time_zones.is_loaded {
            return;
        }
        self.time_zones.is_loaded = true;

        let log_event_string = global.binlog_pmc().get_raw(Self::database_key().as_bytes());
        if log_event_string.is_empty() {
            return;
        }
        match TimeZoneList::parse(&mut log_event_string.as_slice()) {
            Ok(time_zones) => self.time_zones = time_zones,
            Err(status) => {
                tracing::error!("Failed to parse time zones from binlog: {status}");
                self.time_zones = TimeZoneList::default();
            }
        }
    }

    /// `TimeZoneManager::save_time_zones`.
    pub fn save_time_zones(&mut self, global: &mut Global) {
        let mut out = Vec::new();
        self.time_zones.store(&mut out);
        // The C++ stores the binary blob through a std::string; the byte
        // pmc accessors keep it intact.
        global
            .binlog_pmc()
            .set_raw(Self::database_key().as_bytes(), &out);
    }

    /// `TimeZoneManager::get_time_zones` (M.1): serve the pmc-backed list;
    /// the C++ falls through to `reload_time_zones` when the stored hash is
    /// 0 — the query path is the net step, so the empty case answers the
    /// (empty) stored state here.
    pub fn get_time_zones(&mut self, global: &mut Global) -> td_api::TimeZones {
        self.load_time_zones(global);
        self.time_zones.get_time_zones_object()
    }

    /// `TimeZoneManager::on_get_time_zones` — the `help_timezonesList`
    /// application (the query plumbing lands with the net step; tests drive
    /// this directly). Saves only when the list or the hash changed, like
    /// the C++.
    pub fn on_get_time_zones(&mut self, global: &mut Global, time_zones: Vec<TimeZone>, hash: i32) {
        self.load_time_zones(global);
        if self.time_zones.time_zones != time_zones || self.time_zones.hash != hash {
            self.time_zones.time_zones = time_zones;
            self.time_zones.hash = hash;
            self.save_time_zones(global);
        }
        self.time_zones.is_loaded = true;
    }

    /// `TimeZoneManager::get_time_zone_offset` (TimeZoneManager.cpp:113-121):
    /// the stored offset for the id, falling back to the `utc_time_offset`
    /// option (C++ default 0) for unknown ids.
    pub fn get_time_zone_offset(
        &mut self,
        global: &mut Global,
        option_manager: &OptionManager,
        time_zone_id: &str,
    ) -> i32 {
        self.load_time_zones(global);
        for time_zone in &self.time_zones.time_zones {
            if time_zone.id == time_zone_id {
                return time_zone.utc_offset;
            }
        }
        option_manager.get_option_integer("utc_time_offset", 0) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The C++ `td::store` layout: vector count, then
    /// `store(id) store(name) store(i32)` per element, then the hash.
    #[test]
    fn list_layout_matches_cxx_store() {
        let list = TimeZoneList {
            time_zones: vec![TimeZone {
                id: "UTC".to_string(),
                name: "Coordinated Universal Time".to_string(),
                utc_offset: 0,
            }],
            hash: 0x11223344,
            is_loaded: false,
        };
        let mut out = Vec::new();
        list.store(&mut out);

        let mut expected = Vec::new();
        expected.extend_from_slice(&1i32.to_le_bytes());
        // id
        expected.extend_from_slice(&3i32.to_le_bytes());
        expected.extend_from_slice(b"UTC");
        expected.push(0); // padding
        // name (26 bytes + 2 padding)
        let name = b"Coordinated Universal Time";
        expected.extend_from_slice(&(name.len() as i32).to_le_bytes());
        expected.extend_from_slice(name);
        expected.extend_from_slice(&[0, 0]);
        // utc_offset
        expected.extend_from_slice(&0i32.to_le_bytes());
        // hash
        expected.extend_from_slice(&0x11223344i32.to_le_bytes());

        assert_eq!(out, expected);
        // And the round-trip.
        let parsed = TimeZoneList::parse(&mut out.as_slice()).unwrap();
        assert_eq!(
            parsed,
            TimeZoneList {
                is_loaded: true,
                ..list
            }
        );
    }

    #[test]
    fn parse_rejects_truncated_input() {
        assert!(TimeZoneList::parse(&mut &[][..]).is_err());
        let truncated = [
            1i32.to_le_bytes().to_vec(),
            3i32.to_le_bytes().to_vec(),
            b"U".to_vec(),
        ]
        .concat();
        assert!(TimeZoneList::parse(&mut truncated.as_slice()).is_err());
    }

    /// The pmc persistence: save through the Global, reload in a fresh
    /// manager, get the same td_api object.
    #[test]
    fn pmc_round_trip_through_global() {
        let dir = std::env::temp_dir().join(format!("td-rs-tz-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_dir = dir.to_string_lossy().into_owned();

        let zones = vec![
            TimeZone {
                id: "Europe/Moscow".to_string(),
                name: "Moscow".to_string(),
                utc_offset: 10800,
            },
            TimeZone {
                id: "America/Chicago".to_string(),
                name: "Chicago".to_string(),
                utc_offset: -18000,
            },
        ];

        {
            let parameters = tddb::td_db::Parameters {
                database_directory: db_dir.clone(),
                ..Default::default()
            };
            let opened = tddb::td_db::TdDb::open(parameters).unwrap();
            let mut global = Global::new();
            global.init(opened.database);
            let mut manager = TimeZoneManager::default();
            manager.on_get_time_zones(&mut global, zones.clone(), 777);
            let served = manager.get_time_zones(&mut global);
            assert_eq!(served.time_zones.len(), 2);
            assert_eq!(served.time_zones[0].id, "Europe/Moscow");
            assert_eq!(served.time_zones[1].utc_time_offset, -18000);
            global.close_all(false);
        }

        {
            let parameters = tddb::td_db::Parameters {
                database_directory: db_dir,
                ..Default::default()
            };
            let opened = tddb::td_db::TdDb::open(parameters).unwrap();
            let mut global = Global::new();
            global.init(opened.database);
            let mut manager = TimeZoneManager::default();
            let served = manager.get_time_zones(&mut global);
            assert_eq!(served.time_zones.len(), 2);
            assert_eq!(served.time_zones[0].utc_time_offset, 10800);
            global.close_all(false);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Corrupt (non-UTF-8, malformed) pmc data resets the list — including
    /// `is_loaded`, so the next call retries — and never panics on the
    /// byte-exact read.
    #[test]
    fn corrupt_saved_data_resets_the_list() {
        let dir = std::env::temp_dir().join(format!("td-rs-tz-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let parameters = tddb::td_db::Parameters {
            database_directory: dir.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let opened = tddb::td_db::TdDb::open(parameters).unwrap();
        let mut global = Global::new();
        global.init(opened.database);
        global
            .binlog_pmc()
            .set_raw(b"time_zones", &[0xFF, 0xFE, 0x00, 0x01, 0x02]);

        let mut manager = TimeZoneManager::default();
        manager.load_time_zones(&mut global);
        assert!(manager.time_zones.time_zones.is_empty());
        assert!(!manager.time_zones.is_loaded);
        global.close_all(false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `get_time_zone_offset`: the stored offset for a known id, the
    /// `utc_time_offset` option (default 0) for an unknown one.
    #[test]
    fn offset_lookup_falls_back_to_the_option() {
        let dir = std::env::temp_dir().join(format!("td-rs-tz-offset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let parameters = tddb::td_db::Parameters {
            database_directory: dir.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let opened = tddb::td_db::TdDb::open(parameters).unwrap();
        let mut global = Global::new();
        global.init(opened.database);
        let mut manager = TimeZoneManager::default();
        manager.on_get_time_zones(
            &mut global,
            vec![TimeZone {
                id: "Europe/Kyiv".to_string(),
                name: "Kyiv".to_string(),
                utc_offset: 7200,
            }],
            7,
        );

        let option_manager = crate::option_manager::OptionManager::new(false);
        assert_eq!(
            manager.get_time_zone_offset(&mut global, &option_manager, "Europe/Kyiv"),
            7200
        );
        assert_eq!(
            manager.get_time_zone_offset(&mut global, &option_manager, "Mars/Olympus"),
            0
        );
        let mut option_manager = crate::option_manager::OptionManager::new(false);
        option_manager.set_option_integer("utc_time_offset", -18000);
        assert_eq!(
            manager.get_time_zone_offset(&mut global, &option_manager, "Mars/Olympus"),
            -18000
        );
        global.close_all(false);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
