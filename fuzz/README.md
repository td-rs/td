# fuzz

Fuzz targets (S78), one per parsing-heavy component: TL decoders, td_api
JSON, URL, HTML, entities, binlog reader, secret-chat crypto.

## Current targets

| Target            | Surface under test                                                       |
| ----------------- | ------------------------------------------------------------------------ |
| `td_api_json`     | `ClientJson::to_request` — `td_execute` JSON + `@extra` + Function parse |
| `json_value`      | `tdutils::json` raw JSON parser                                          |
| `binlog_event`    | `BinlogEvent::init` — the byte-identical S40 record format               |
| `mtproto_packet`  | `packet::read_with_x` — decrypt + msg_id/seq_no + body under a fixed key |
| `tl_scheme_text`  | `tdtl` lexer + parse tree — the codegen front end                        |
| `transport_frame` | stream transport framing (Tcp/ObfuscatedTcp/Http) via `create_transport` |
| `app_config`     | `ConfigManager::process_app_config` — app-config JSON application over every key map (M.3.10.3) |
| `simple_config`  | `decode_config` — simple-config RSA/AES/SHA rejection space (M.3.10.3) |
| `user_log_event`  | the `User` database blob — Users binlog events + `us<id>` rows (both framings) |
| `user_full`       | the `UserFull` database blob — `usf<id>` rows                            |
| `secret_chat`     | the `SecretChat` database blob — SecretChats binlog events + `sc<id>` rows |
| `emoji_status`    | the `EmojiStatus` value parser — the payload inside `User` blobs         |

The M.4.17 database-blob targets (`user_log_event`, `user_full`,
`secret_chat`, `emoji_status`) cover what is attacker-reachable from the
`td` directory: every byte parsed there replays on client start from
on-disk state. Pending: the photo.rs value-parser target arrives with
M.4.13; URL, HTML, entities and the secret-chat crypto targets arrive
with their Track I components.
The S78 release gate — one week fuzz-clean per target — runs against the
full set once Track I lands those.

## Usage

The crate is excluded from the workspace and builds standalone
(`libfuzzer-sys` bundles the libFuzzer runtime; any C++ compiler works):

```sh
cd fuzz
cargo build                                        # uninstrumented smoke builds
cargo fuzz run td_api_json -- -max_total_time=60   # instrumented (nightly + cargo-fuzz)
```

CI: `fuzz-smoke` job in `.github/workflows/main.yml` — 60 s per target on
nightly via cargo-fuzz, as a regression tripwire. Panic = bug: every
parser here must report errors, never crash (the tdlib parser contract).

Findings so far: the initial `transport_frame` harness (before the
write→read alternation was modeled) tripped the transports' turn asserts —
expected, the C++ `CHECK`s do the same; no parser panics found to date.
