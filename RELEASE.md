# Release process (S84)

The plan's S84: *version sync with upstream scheme imports;
changelog-driven parity audit each release; verify — parity checklist
green.* This runbook is the procedure; every step is a command that
already exists in the repo. Full S84 activation waits on the S75 corpus
CI running nightly (landed on the `track-h` branch) and the S76 gate.

## 1. Scheme import sync (per upstream release, D5: monthly cadence)

1. Fetch the new upstream `tdlib/td` and identify the release commit.
2. Copy the five schemes into `crates/tl-codegen/scheme/` and refresh
   `sha256sums.txt`:
   ```sh
   cd crates/tl-codegen/scheme
   sha256sum -b td_api.tl telegram_api.tl secret_api.tl e2e_api.tl mtproto_api.tl > sha256sums.txt
   ```
3. Audit the diff (this is the deliberate, visible pin change):
   `git diff sha256sums.txt` — record the upstream commit in the message.
4. Verify the import against the C++ oracle:
   `conformance/cpp_dump/verify.sh <td-checkout>` (S04 golden vectors) and
   `conformance/scheme_parity/check.sh <td-checkout>` (per-scheme drift).

## 2. Regenerate + differential gates

```sh
cargo run -p tl-codegen --bin sync-generated  # regenerate the committed
                                  # src/generated/ modules (the
                                  # generated_drift test gates drift)
cargo build --workspace
cargo test --workspace --locked   # round-trips, vectors, ctor-id parity
conformance/tl_oracle/verify_scheme.sh <td-checkout>   # S07 counts/dump
conformance/tl_store_oracle/verify.sh <td-checkout>    # S10 binary store
conformance/tl_json_oracle/verify.sh <td-checkout>     # S12 JSON format
conformance/tl_replay_oracle/verify.sh <td-checkout>   # S15 byte-equality
```

Scheme-count drift (types/constructors/functions vs the S07 dump) is the
signal for new-API work: every new constructor needs a store/parse path
and, for td_api, JSON conformance before the release candidate is cut.

## 3. Parity audit (the S84 checklist)

The progress metric from plan §5 risk 6 — *functions implemented / 1,022
× conformance sessions green*:

```sh
conformance/target/<profile>/td-api-parity    # count + per-function list
```

A release is parity-clean when **all** of the following are green:

- [ ] `td-api-parity`: implemented = 1,022 / 1,022
- [ ] every S75 corpus session replays clean (replayer diffed vs C++)
- [ ] all S73/S74/S68 oracle gates green in CI (`fuzz-smoke`,
      `conformance-verify`, `fuzz` jobs in the workflows)
- [ ] S78 weekly fuzz audit clean: `grep FINDING fuzz/fuzz-week.log`
- [ ] S37/S79 bench gates: no regression past the documented 2× rows
      (`bench/README.md` tables; re-run the flagged-row protocol on a
      quiet machine)
- [ ] S80 soak: `soak 3600` PASS
- [ ] cargo-deny advisories clean (or every exception explicitly signed
      off — see the open `rsa` RUSTSEC-2023-0071 decision)

Current status (2026-11-09, the 1.8.70 upstream sync over branch
`upstream-1.8.70` @ 825dacc + docs, pinned upstream
`23383aef218a3123f02b744bfa91bde21056a814` / tdlib 1.8.70, layer 233):
the census reads 26 sync / 1,040 dispatched / 33 interim of 1,099 arms
and every differential gate ran green against a pristine 23383aef C++
checkout — scheme_parity (local + upstream drift), the S04/S06/S07/S08/
S10/S12/S15 source oracles, the S68 `tdjson_oracle` over a fresh Release
`TD_BUILD` (`libtdjson.so.1.8.70`; 37 lines byte-identical),
`requests_gate`, the 14/14/0 symbol audit, and byte-identical vendored
headers. Workspace gates green on both machines (fmt, clippy 1.95,
rustdoc `-D warnings`, `cargo test --workspace --locked` 9,045/0/3).
cargo-deny green carrying the two pinned ignores (RUSTSEC-2023-0071 rsa,
RUSTSEC-2026-0386 tokio; `deny.toml` unchanged — 1.54.1 still regresses
the tdcli exit path). The 33 interim arms are the sync's recorded
divergence — one follow-up family issue each (§4's 1.8.70 entry).
Deferred per owner decision 2026-11-09, restated at the sync: soak 3600,
the bench flagged-row re-run, the S75 corpus replay (TD_RECORD_* secrets),
and the fuzz week (skipped since the 09-27 decision) — 0.1.x follow-ups,
not parity claims.

## 4. Changelog + version

- One changelog entry per release: scheme-commit bump, manager waves
  landed (Track I), gates re-run with dates, known divergences (keep the
  `bench/README.md` divergence sections in sync).
- Version: the workspace `version = "0.1.0"` bumps on API-visible
  milestones (S76 walking skeleton → 0.1.0 tag with the first release
  artifacts; S81 bindings validation → 0.2.0; S84 first full-parity
  release → 1.0.0-candidate).
- **2026-11-09 upstream sync (TDLib 1.8.68 → 1.8.70)**, pin bump
  `c15d3f5a5de6e3ba5839822c451152e5e18bb700` →
  `23383aef218a3123f02b744bfa91bde21056a814` (tdlib 1.8.70, layer 233),
  branch `upstream-1.8.70` (spec td-rs/td#109; tickets #110–#117): the
  five-scheme sha256 pin moved as one deliberate change, the committed
  generated modules regenerated (`generated_drift` green), the layer
  constant and version strings moved 230/1.8.68 → 233/1.8.70:
  - Oracle set green against a pristine 23383aef C++ worktree plus a
    fresh Release `TD_BUILD` (`libtdjson.so.1.8.70`), Desktop 2026-11-09:
    `cpp_dump` (S04, 5/5 blobs), `tl_oracle` tokens/scheme/tlo (S06
    89,459 tokens over 5 schemes; S07 td_api 818 types / 2,308
    constructors / 1,099 functions; S08 `.tlo` bytes identical),
    `tl_store_oracle` (S10, 10/10 over the regenerated layer-233
    constructors), `tl_json_oracle` (S12, 37/37), `tl_replay_oracle`
    (S15, 5,552 samples, 0 failures), `tdjson_oracle` (S68, 37/37 vs the
    C++ `libtdjson.so.1.8.70`).
  - `requests_gate` green; census **1,040 dispatched / 33 interim / 26
    sync of 1,099** — the spec's floor was 1,039/34/26, and #113's
    promotion of `setCommunityNotificationSettings` moved one interim
    arm to dispatched (+1/−1, the reason the changelog census differs
    from the spec's). m7 fan-out 56 C++ intakes = 2 live / 3 seam / 51
    documented drop; `verify_symbols.sh` 14 required / 14 exported /
    0 extra; vendored headers 2/2 byte-identical; the tdjson surface
    answers `version = "1.8.70"` at layer 233 byte-exact.
  - Triage of the 298 upstream commits: 86 ported (46 by the family
    tickets — #112 ephemeral 17, #113 community-notif 17, #117 quotes
    12 — and 40 by the #114 predecessor + sweep commits), ~98 absorbed
    at the sync base or by the scheme regen, ~114 site-less with
    reasons — the sync REMAINING doc is the canonical record
    (`docs/UPSTREAM_SYNC_23383AEF_REMAINING_20261109.md`, git-excluded
    by design; the PR body carries its digest).
  - Workspace verification green on both machines 2026-11-09 (Desktop
    and ThinkPad, rustup 1.95): fmt, clippy `-D warnings`, rustdoc
    `-D warnings`, `cargo test --workspace --locked` — 56 suites,
    9,045 passed / 0 failed / 3 ignored (the ThinkPad's first test run
    hit the known tdcli harness load flake; solo rerun + full rerun
    green). cargo-deny green (advisories/bans/licenses/sources) with no
    new exceptions.
  - Deferred restated (same set as the 0.1.0 audit, owner decision
    2026-11-09): soak 3600, bench flagged rows, S75 corpus replay, fuzz
    week. CI conformance pin moved `c15d3f5a` → `23383aef`
    (`main.yml` ×2, AGENTS.md); README tracks 1.8.70. No crate version
    bump, no tag (the workspace stays 0.1.0; versions bump on release
    milestones).
- **2026-11-09 release audit (the 0.1.0 tag)**, against the pinned
  upstream `c15d3f5a5de6e3ba5839822c451152e5e18bb700` (tdlib 1.8.68,
  layer 230), using a pristine td worktree at the pin plus a fresh
  Release `TD_BUILD`:
  - `conformance/scheme_parity/check.sh` — the five schemes
    byte-identical to the pin (local + upstream drift).
  - Oracle set green: `cpp_dump` (S04), `tl_oracle` tokens/scheme/tlo
    (S06/S07/S08), `tl_store_oracle` (S10, over the regenerated
    layer-230 constructors), `tl_json_oracle` (S12),
    `tl_replay_oracle` (S15), `tdjson_oracle` (S68, 37 lines
    byte-identical against the C++ `libtdjson.so.1.8.68`).
  - `requests_gate` green; census 1,039 dispatched / 0 interim / 26 sync
    of 1,065; `verify_symbols.sh` 14 required / 14 exported / 0 extra;
    vendored headers byte-identical; the tdjson surface answers
    `version = "1.8.68"` byte-exact.
  - Workspace verification green on the tagged tree (fmt, clippy 1.95,
    rustdoc `-D warnings`, `cargo test --workspace --locked` — 8,851
    passed / 0 failed).
  - Advisory wave cleared: rustls 0.23.46, rustls-webpki 0.103.16,
    webpki-roots 1.0.11, libz-sys 1.1.32, and crypto-bigint 0.6→0.7.11
    (the RUSTSEC-2026-0387 SRP/DH modpow migration in
    `tdutils/bignum.rs`). tokio pins 1.53.1: 1.54.1 regresses the tdcli
    exit path (four harness cases hang), and RUSTSEC-2026-0386 has no
    `AsyncFd` call site in the tree — the exception and the regression
    are recorded in `deny.toml`.
  - Deferred per owner decision 2026-11-09: soak 3600, bench flagged
    rows, S75 corpus replay (needs the TD_RECORD_* secrets); the fuzz
    week stays skipped (09-27 decision). CI conformance pin moved
    `d1085f9c` → `c15d3f5a` (`main.yml` ×2, AGENTS.md).
- **2026-09-23 changelog-parity audit (S84)**, against the pinned upstream
  `d1085f9cebc5a62379991ae1652673954f229c1f` (tdlib 1.8.67):
  - `conformance/scheme_parity/check.sh` — the five schemes match the S03
    sha256 pin (local drift green; the upstream drift check runs at import
    time per §1 with a td checkout).
  - The tdjson surface reports `version = "1.8.67"` byte-identical to the
    C++ (`td_execute getOption version`, asserted by the
    `execute_get_option_round_trip` test and the S81 python run).
  - The S81 binding matrix (`conformance/bindings/README.md`), the S83
    symbol audit (`conformance/verify_symbols.sh`: 14 required, 14
    exported, 0 extra) and the request census above are the
    release-candidate evidence set; `conformance/requests_gate/verify.sh`
    green on the same tree.
  - Known divergences live in `conformance/bindings/README.md` (close
    latency after a pre-auth config fetch; the mixed legacy+manager
    teardown fatal) and `bench/README.md`.

## 5. Artifacts

S82 packaging is **activated for Linux** as of 2026-09-23. The release
artifact set matches a tdlib install: the `tdjson` cdylib plus the C
headers it ships.

- `libtdjson.so` — `cargo build --release -p tdjson` (the `release.yml`
  glob picks it up on `v*` tags; CI remains gated on the billing fix).
- `include/td/telegram/td_json_client.h`, `include/td/telegram/td_log.h` —
  vendored byte-identical from the pinned upstream commit under
  `crates/tdjson/include/` (`tdjson_export.h` is a CMake-generated C++
  visibility header with no consumer outside td's own build and is
  deliberately not shipped).
- Verified as a drop-in: the S81 python example runs against exactly this
  staged pair (`conformance/bindings/python_smoke.py`), and
  `conformance/verify_symbols.sh` pins the export surface.
- **wasm (example/web) status**: the `wasm32` CI job stays advisory. The
  workspace does not yet check clean for `wasm32-unknown-unknown`:
  `getrandom 0.2.17` fails with "the wasm*-unknown-unknown targets are not
  supported by default, you may need to enable the `js` feature". S82's
  remaining wasm work: a target-scoped `getrandom` `js`-feature wiring,
  then an emscripten (`wasm32-unknown-emscripten`) cdylib build feeding
  the `tdweb` packaging scripts.
