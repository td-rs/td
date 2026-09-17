//! Port of the ConfigManager result-routing half of the net dispatch
//! (M.3.8.2): the `ConfigManager::on_result` net entry
//! (ConfigManager.cpp:1040-1140) — a completed `NetQuery` plus its
//! `actor_shared` link token decodes into a
//! [`ConfigQueryResult`](crate::config_manager::ConfigQueryResult) and feeds
//! the token dispatch in [`Td::on_config_query_result`](crate::td::Td).
//!
//! The decode is the C++ `fetch_result<T>` (NetQuery.h:339-362): a
//! query-level error propagates as-is; the ok bytes parse as the expected
//! answer type with parse failures wrapped as `Status::Error(500, …)`
//! (NetQuery.h:345-349). `rpc_error` answers never reach this layer — the
//! mtproto session connection converts them to query errors before the
//! completion is delivered (SessionConnection.cpp:256-261).
//!
//! The close half mirrors the dispatcher stop: `Td::clear` stops the
//! `NetQueryDispatcher` (Td.cpp:663), whose stop flag completes every
//! still-dispatched query with `Global::request_aborted_error()`
//! (NetQueryDispatcher.cpp:49-55, Global.h:701-703); the completions flow
//! back through `on_result`, which fails the manager's pending promise
//! queues before it drops.

use tdmtproto::telegram_api;
use tdnet::net_query::NetQuery;
use tdutils::status::Status;

use crate::config_manager::{AppConfigQueryResult, ConfigQuery, ConfigQueryResult};

/// The reverse of the M.3.8.1 dispatch — map a completion's
/// `actor_shared` link token back to the dispatched query, validating
/// against the request bytes (the provenance the C++ link token
/// guarantees): the token selects the kind the `ConfigManager::on_result`
/// switches on (ConfigManager.cpp:1042/:1066/:1080/:1120) and the request
/// constructor must match it; the `GetAppConfig` hash re-parses from the
/// request body. Returns `None` for mismatched pairs (and the token-0
/// callback-less `TdSink` stand-in).
pub(crate) fn reconstruct_config_query(token: u64, net_query: &NetQuery) -> Option<ConfigQuery> {
    let request = net_query.query();
    if request.len() < 4 {
        return None;
    }
    let constructor = i32::from_le_bytes(request[0..4].try_into().unwrap());
    Some(match token {
        // 8/9 = help.getConfig (9 = with reopen-sessions).
        8 | 9 if constructor == telegram_api::help_getConfig::ID => ConfigQuery::GetConfig {
            dc_id: net_query.dc_id().get_value(),
            reopen_sessions: token == 9,
        },
        // 1 = help.getAppConfig.
        1 if constructor == telegram_api::help_getAppConfig::ID => ConfigQuery::GetAppConfig {
            hash: telegram_api::help_getAppConfig::parse(request).ok()?.hash,
        },
        // 2 = account.getContentSettings.
        2 if constructor == telegram_api::account_getContentSettings::ID => {
            ConfigQuery::GetContentSettings
        }
        // 3/4 = account.setContentSettings (4 = enabling the flag).
        3 | 4 if constructor == telegram_api::account_setContentSettings::ID => {
            ConfigQuery::SetContentSettings {
                sensitive_enabled: token == 4,
            }
        }
        _ => return None,
    })
}

/// `boolTrue#997275b5` / `boolFalse#bc799737` (telegram_api.tl:11-12) —
/// the scheme-builtin `Bool` is not generated as a type; the two boxed
/// wire constructors are all `fetch_result` of a `Bool`-returning function
/// needs.
const BOOL_TRUE_ID: i32 = 0x9972_75b5u32 as i32;
const BOOL_FALSE_ID: i32 = 0xbc79_9737u32 as i32;

/// The generated per-type boxed `parse` behind one name, so
/// [`fetch_boxed`] can be generic over the four answer types.
trait BoxedParse: Sized {
    fn parse_boxed(input: &[u8]) -> Result<Self, &'static str>;
}

macro_rules! boxed_parse {
    ($($ty:ty),* $(,)?) => {
        $(
            impl BoxedParse for $ty {
                fn parse_boxed(input: &[u8]) -> Result<Self, &'static str> {
                    // The generated inherent boxed `parse` — no ambiguity
                    // with the trait's `parse_boxed`.
                    Self::parse(input)
                }
            }
        )*
    };
}

boxed_parse!(
    telegram_api::Config,
    telegram_api::account_ContentSettings,
    telegram_api::help_appConfig
);

/// `fetch_result<T>` (NetQuery.h:354-362).
fn fetch_boxed<T: BoxedParse>(net_query: &mut NetQuery) -> Result<T, Status> {
    if net_query.is_error() {
        return Err(net_query.move_as_error());
    }
    let bytes = net_query.move_as_ok();
    match T::parse_boxed(&bytes) {
        Ok(value) => Ok(value),
        Err(error) => {
            // NetQuery.h:345-349: the parse failure is logged and wrapped
            // as a 500.
            tracing::error!("Can't parse net query result: {error}");
            Err(Status::error(500, error))
        }
    }
}

/// The `help.getAppConfig` answer split — the C++ `on_result` token-1
/// branch (ConfigManager.cpp:1093-1111) distinguishes
/// `help.appConfigNotModified` from `help.appConfig` by the answer
/// constructor before the typed parse.
fn decode_app_config_answer(bytes: &[u8]) -> Result<AppConfigQueryResult, Status> {
    if bytes.len() >= 4
        && i32::from_le_bytes(bytes[0..4].try_into().unwrap())
            == telegram_api::help_appConfigNotModified::ID
    {
        return Ok(AppConfigQueryResult::NotModified);
    }
    match telegram_api::help_appConfig::parse(bytes) {
        Ok(app_config) => Ok(AppConfigQueryResult::Config {
            hash: app_config.hash,
            config: app_config
                .config
                .expect("help.appConfig always carries a config"),
        }),
        Err(error) => {
            tracing::error!("Can't parse app config answer: {error}");
            Err(Status::error(500, error))
        }
    }
}

/// The `Bool` answer of `account.setContentSettings` — the C++
/// `fetch_result` parses the boxed `Bool` and the caller discards the
/// value (ConfigManager.cpp:1045-55); only the parse itself is enforced.
fn decode_bool_answer(bytes: &[u8]) -> Result<(), Status> {
    if bytes.len() < 4 {
        return Err(Status::error(500, "Unexpected end"));
    }
    let constructor = i32::from_le_bytes(bytes[0..4].try_into().unwrap());
    if constructor == BOOL_TRUE_ID || constructor == BOOL_FALSE_ID {
        return Ok(());
    }
    Err(Status::error(500, "Wrong constructor"))
}

/// The `ConfigManager::on_result` decode: one completed net query → the
/// typed [`ConfigQueryResult`] its query kind expects. The `query` selects
/// the expected answer type exactly like the C++ link token does (the two
/// carry the same information — see [`reconstruct_config_query`]).
pub(crate) fn decode_config_query_result(
    query: &ConfigQuery,
    net_query: &mut NetQuery,
) -> ConfigQueryResult {
    debug_assert!(net_query.is_ready(), "decoding an unanswered query");
    match query {
        ConfigQuery::GetConfig { .. } => {
            ConfigQueryResult::GetConfig(fetch_boxed::<telegram_api::Config>(net_query))
        }
        ConfigQuery::GetAppConfig { .. } => {
            if net_query.is_error() {
                ConfigQueryResult::GetAppConfig(Err(net_query.move_as_error()))
            } else {
                let bytes = net_query.move_as_ok();
                ConfigQueryResult::GetAppConfig(decode_app_config_answer(&bytes))
            }
        }
        ConfigQuery::GetContentSettings => {
            ConfigQueryResult::GetContentSettings(fetch_boxed::<
                telegram_api::account_ContentSettings,
            >(net_query))
        }
        ConfigQuery::SetContentSettings { .. } => {
            if net_query.is_error() {
                ConfigQueryResult::SetContentSettings(Err(net_query.move_as_error()))
            } else {
                let bytes = net_query.move_as_ok();
                ConfigQueryResult::SetContentSettings(decode_bool_answer(&bytes))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdnet::net_query::{AuthFlag, DcId, GzipFlag, NetQueryType};

    /// A completed query carrying `answer` — the dispatch+session pipeline
    /// hand the callback a ready query; tests short-circuit to the
    /// completed form.
    fn answered(id: u64, query: &ConfigQuery, answer: Vec<u8>) -> NetQuery {
        let bytes = query.serialize();
        let tl_constructor = i32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let mut net_query = NetQuery::new(
            id,
            DcId::main(),
            NetQueryType::Common,
            AuthFlag::Off,
            GzipFlag::Off,
            tl_constructor,
            bytes,
            u32::MAX,
            60,
        );
        net_query.set_ok(answer);
        net_query
    }

    fn failed(id: u64, query: &ConfigQuery, status: Status) -> NetQuery {
        let mut net_query = answered(id, query, Vec::new());
        net_query.set_error(status);
        net_query
    }

    /// The token → query reconstruction over the four kinds, and the
    /// mismatch drops.
    #[test]
    fn reconstructs_the_dispatched_query_from_token_and_bytes() {
        let cases = [
            (
                8u64,
                ConfigQuery::GetConfig {
                    dc_id: 1,
                    reopen_sessions: false,
                },
            ),
            (
                9,
                ConfigQuery::GetConfig {
                    dc_id: 1,
                    reopen_sessions: true,
                },
            ),
            (1, ConfigQuery::GetAppConfig { hash: 7 }),
            (2, ConfigQuery::GetContentSettings),
            (
                3,
                ConfigQuery::SetContentSettings {
                    sensitive_enabled: false,
                },
            ),
            (
                4,
                ConfigQuery::SetContentSettings {
                    sensitive_enabled: true,
                },
            ),
        ];
        for (token, query) in &cases {
            let net_query = answered(11, query, Vec::new());
            let reconstructed = reconstruct_config_query(*token, &net_query)
                .unwrap_or_else(|| panic!("reconstruct {query:?}"));
            match (query, &reconstructed) {
                // The dc_id rides the query's own DcId (main()); the
                // router ignores it.
                (
                    ConfigQuery::GetConfig {
                        reopen_sessions: a, ..
                    },
                    ConfigQuery::GetConfig {
                        reopen_sessions: b, ..
                    },
                ) => assert_eq!(a, b, "{query:?}"),
                _ => assert_eq!(&reconstructed, query),
            }
        }

        // Mismatched pairs drop: wrong token for the request bytes, the
        // token-0 callback-less stand-in, and short bodies.
        let get_config = answered(11, &ConfigQuery::GetContentSettings, Vec::new());
        assert!(reconstruct_config_query(1, &get_config).is_none());
        assert!(reconstruct_config_query(0, &get_config).is_none());
        let short = NetQuery::new(
            12,
            DcId::main(),
            NetQueryType::Common,
            AuthFlag::Off,
            GzipFlag::Off,
            0,
            vec![1, 2, 3],
            u32::MAX,
            60,
        );
        assert!(reconstruct_config_query(8, &short).is_none());
    }

    #[test]
    fn fetch_result_decodes_all_four_answer_kinds() {
        // help.getConfig → config (the td.rs option-broadcast fixture);
        // `flags: 4` = flags.2 carries suggested_lang_code/lang_pack_version
        // like a real server answer.
        let config = telegram_api::Config {
            flags: 4,
            this_dc: 1,
            date: 1000,
            expires: 2000,
            test_mode: false,
            webfile_dc_id: 4,
            tmp_sessions: 100,
            edit_time_limit: 172800,
            suggested_lang_code: "en".to_string(),
            lang_pack_version: 5,
            caption_length_max: 5000,
            message_length_max: 50000,
            me_url_prefix: "https://t.me".to_string(),
            ..Default::default()
        };
        let mut bytes = Vec::new();
        config.store_boxed(&mut bytes);
        let mut net_query = answered(
            1,
            &ConfigQuery::GetConfig {
                dc_id: 1,
                reopen_sessions: false,
            },
            bytes,
        );
        let ConfigQueryResult::GetConfig(Ok(decoded)) = decode_config_query_result(
            &ConfigQuery::GetConfig {
                dc_id: 1,
                reopen_sessions: false,
            },
            &mut net_query,
        ) else {
            panic!("GetConfig decode");
        };
        assert_eq!(decoded.date, config.date);
        assert_eq!(decoded.suggested_lang_code, config.suggested_lang_code);
        assert_eq!(decoded.lang_pack_version, config.lang_pack_version);
        assert_eq!(decoded.me_url_prefix, config.me_url_prefix);

        // help.getAppConfig → help.appConfig{hash, config}.
        let app_config = telegram_api::help_appConfig {
            hash: -12345,
            config: Some(Box::new(telegram_api::JSONValue::jsonNull(
                telegram_api::jsonNull,
            ))),
        };
        let mut bytes = Vec::new();
        app_config.store_boxed(&mut bytes);
        let query = ConfigQuery::GetAppConfig { hash: 0 };
        let mut net_query = answered(2, &query, bytes);
        let ConfigQueryResult::GetAppConfig(Ok(AppConfigQueryResult::Config { hash, config })) =
            decode_config_query_result(&query, &mut net_query)
        else {
            panic!("GetAppConfig decode");
        };
        assert_eq!(hash, -12345);
        assert!(matches!(*config, telegram_api::JSONValue::jsonNull(_)));

        // help.getAppConfig → help.appConfigNotModified.
        let mut bytes = Vec::new();
        telegram_api::help_appConfigNotModified.store_boxed(&mut bytes);
        let mut net_query = answered(3, &query, bytes);
        assert!(matches!(
            decode_config_query_result(&query, &mut net_query),
            ConfigQueryResult::GetAppConfig(Ok(AppConfigQueryResult::NotModified))
        ));

        // account.getContentSettings → account.contentSettings (both bools
        // are `?true` conditionals the store folds into the flags word).
        let content_settings = telegram_api::account_ContentSettings {
            flags: 0,
            sensitive_enabled: true,
            sensitive_can_change: false,
        };
        let mut bytes = Vec::new();
        content_settings.store_boxed(&mut bytes);
        let query = ConfigQuery::GetContentSettings;
        let mut net_query = answered(4, &query, bytes);
        let ConfigQueryResult::GetContentSettings(Ok(decoded)) =
            decode_config_query_result(&query, &mut net_query)
        else {
            panic!("GetContentSettings decode");
        };
        assert!(decoded.sensitive_enabled);
        assert!(!decoded.sensitive_can_change);

        // account.setContentSettings → Bool (value discarded).
        for (constructor, name) in [(BOOL_TRUE_ID, "boolTrue"), (BOOL_FALSE_ID, "boolFalse")] {
            let query = ConfigQuery::SetContentSettings {
                sensitive_enabled: true,
            };
            let mut net_query = answered(5, &query, constructor.to_le_bytes().to_vec());
            assert!(
                matches!(
                    decode_config_query_result(&query, &mut net_query),
                    ConfigQueryResult::SetContentSettings(Ok(()))
                ),
                "{name} is a successful set"
            );
        }
    }

    #[test]
    fn fetch_result_propagates_query_errors_verbatim() {
        let status = Status::error(420, "FLOOD_WAIT_60");
        let queries = [
            ConfigQuery::GetConfig {
                dc_id: 1,
                reopen_sessions: false,
            },
            ConfigQuery::GetAppConfig { hash: 0 },
            ConfigQuery::GetContentSettings,
            ConfigQuery::SetContentSettings {
                sensitive_enabled: false,
            },
        ];
        for query in &queries {
            let mut net_query = failed(9, query, status.clone());
            let error = match decode_config_query_result(query, &mut net_query) {
                ConfigQueryResult::GetConfig(Err(error))
                | ConfigQueryResult::GetAppConfig(Err(error))
                | ConfigQueryResult::GetContentSettings(Err(error))
                | ConfigQueryResult::SetContentSettings(Err(error)) => error,
                _ => panic!("{query:?} must propagate the query error"),
            };
            assert_eq!(error.code(), 420);
            assert_eq!(error.message(), "FLOOD_WAIT_60");
        }
    }

    #[test]
    fn fetch_result_wraps_parse_failures_as_500() {
        // A wrong answer constructor fails the typed parse; the C++
        // wraps it as 500 (NetQuery.h:345-349).
        let mut bytes = Vec::new();
        telegram_api::help_appConfigNotModified.store_boxed(&mut bytes);
        let query = ConfigQuery::GetConfig {
            dc_id: 1,
            reopen_sessions: false,
        };
        let mut net_query = answered(1, &query, bytes);
        let ConfigQueryResult::GetConfig(Err(error)) =
            decode_config_query_result(&query, &mut net_query)
        else {
            panic!("GetConfig parse failure");
        };
        assert_eq!(error.code(), 500);

        // The app-config split errors on unknown constructors too.
        let query = ConfigQuery::GetAppConfig { hash: 0 };
        let mut net_query = answered(2, &query, vec![0xde, 0xad, 0xbe, 0xef]);
        let ConfigQueryResult::GetAppConfig(Err(error)) =
            decode_config_query_result(&query, &mut net_query)
        else {
            panic!("GetAppConfig parse failure");
        };
        assert_eq!(error.code(), 500);

        // A non-Bool answer to setContentSettings fails the Bool parse.
        let query = ConfigQuery::SetContentSettings {
            sensitive_enabled: false,
        };
        let mut bytes = Vec::new();
        telegram_api::account_getContentSettings.store_boxed(&mut bytes);
        let mut net_query = answered(3, &query, bytes);
        let ConfigQueryResult::SetContentSettings(Err(error)) =
            decode_config_query_result(&query, &mut net_query)
        else {
            panic!("SetContentSettings parse failure");
        };
        assert_eq!(error.code(), 500);
    }

    #[test]
    fn aborted_completion_carries_the_cpp_stop_error() {
        // The error the dispatcher-stop abort produces (check_stop_flag →
        // request_aborted_error, NetQueryDispatcher.cpp:49-55) propagates
        // verbatim through the decode.
        let query = ConfigQuery::GetAppConfig { hash: 5 };
        let mut net_query = failed(42, &query, tdnet::net_query::request_aborted_error());
        assert_eq!(net_query.id(), 42);
        let ConfigQueryResult::GetAppConfig(Err(error)) =
            decode_config_query_result(&query, &mut net_query)
        else {
            panic!("abort decode");
        };
        assert_eq!(error.code(), 500);
        assert_eq!(error.message(), "Request aborted");
    }
}
