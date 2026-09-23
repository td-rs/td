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
cargo build --workspace           # build.rs regenerates all schemes
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

Current status (2026-09-23, Track J audit): **633 / 1,022 (61.9%)** —
`td-api-parity` counts `Function::<name>` handler references across the
crate sources; the request census pins the same tree at
20 sync / 558 dispatched / 444 interim of 1,022 arms
(`crates/tdclient/src/testdata/request_census.txt`). The number is the
release-blocking metric, not a quality one (the implemented subset is
oracle-verified).

## 4. Changelog + version

- One changelog entry per release: scheme-commit bump, manager waves
  landed (Track I), gates re-run with dates, known divergences (keep the
  `bench/README.md` divergence sections in sync).
- Version: the workspace `version = "0.1.0"` bumps on API-visible
  milestones (S76 walking skeleton → 0.1.0 tag with the first release
  artifacts; S81 bindings validation → 0.2.0; S84 first full-parity
  release → 1.0.0-candidate).
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
