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

Current status (2026-09-15): **13 / 1,022 (1.3%)** — Track I has not
started; the number is the release-blocking metric, not a quality one
(the implemented subset is oracle-verified).

## 4. Changelog + version

- One changelog entry per release: scheme-commit bump, manager waves
  landed (Track I), gates re-run with dates, known divergences (keep the
  `bench/README.md` divergence sections in sync).
- Version: the workspace `version = "0.1.0"` bumps on API-visible
  milestones (S76 walking skeleton → 0.1.0 tag with the first release
  artifacts; S81 bindings validation → 0.2.0; S84 first full-parity
  release → 1.0.0-candidate).

## 5. Artifacts

Packaging (tdjson cdylib + headers per platform, wasm) is S82 — its
`release.yml` workflow exists and activates on `v*` tags once S81's
bindings validation passes.
