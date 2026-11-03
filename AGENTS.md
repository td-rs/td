# AGENTS.md

## What this repo is

From-scratch Rust rewrite of TDLib as a **drop-in replacement** for the C++
`libtdjson`: the `tdjson` cdylib must export **exactly** the 14 official C
symbols (nothing more), ship byte-identical headers
(`crates/tdjson/include/`), and answer identical JSON. Correctness is pinned
against the C++ reference build, not reimplemented from memory — hence the
oracle/conformance gates. The C++ repo is expected as a sibling checkout at
`../td` (it also holds `RUST_REWRITE_PLAN.md`; one green commit per plan
step). `cargo build --release -p tdjson` is the whole artifact story.

## Verification

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps --locked
cargo test --workspace --locked
```

- Every CI command uses `--locked`; keep `Cargo.lock` in sync, don't leave it churned.
- Clippy runs with `-D warnings`: fix warnings, don't allow them. Formatting: `max_width = 100` (rustfmt.toml).
- `.cargo/config.toml` sets `RUST_MIN_STACK=33554432` for all cargo-spawned
  processes — some tests recurse deeply in debug builds. Only set it manually
  when running a test binary directly.
- Toolchain pinned to 1.95 (`rust-toolchain.toml` = the `clippy.toml` MSRV).
  Bumping it is a deliberate, visible change; CI pins it separately via
  `RUSTUP_TOOLCHAIN` and also tests current stable.

## Layout and boundaries

- `crates/*` mirror upstream td module names: `tdutils`, `tdactor`, `tdnet`,
  `tddb`, `tdmtproto`, `tde2e`, `tdclient`, `tdjson` (the C-ABI cdylib),
  `tdtl` (TL parser), `tl-codegen` (generator), `tdcli`.
- **Standalone crates, excluded from the workspace** (own manifests/locks):
  `conformance/` (run cargo from that directory), `fuzz/` (`cargo fuzz` must
  run from the **repo root**, not `fuzz/`), `bench/`, `vendor/`.
- `vendor/` holds patched third-party forks wired via `[patch.crates-io]`
  (libsqlite3-sys emscripten fix); path deps only, never built standalone.

## Generated code — do not hand-edit

- Scheme code is committed at `crates/tdclient/src/generated/`,
  `crates/tdmtproto/src/generated/`, `crates/tde2e/src/generated/` and
  compiled directly; `tl-codegen` is a dev tool, not a build dependency.
- After changing the schemes, the model, or the writer, regenerate with
  `cargo run -p tl-codegen --bin sync-generated`. The `generated_drift` test
  fails CI when committed files drift from the generator.
- The five `.tl` schemes in `crates/tl-codegen/scheme/` are pinned by
  `sha256sums.txt`; importing a new upstream scheme is a deliberate pin
  change (procedure in `RELEASE.md` §1).

## Conformance / oracles

- Differential gates: `conformance/*/verify.sh <path-to-td-checkout>`, pinned
  to upstream commit `d1085f9cebc5a62379991ae1652673954f229c1f` (see
  `.github/workflows/main.yml`). They need a C++ td build (cmake, gperf,
  php) — slow, so they run in Main CI, not PR CI.
- Live-session recording requires the `TD_BOT_TOKEN` / `TD_BOT_USERNAME` /
  `TD_API_HASH` / `TD_PEER_STATE` secrets. Recorded corpus files
  (`conformance/recorder/corpus/s76_*`) contain the bot token — never commit
  them (gitignored).

## Build environment

- Vendored OpenSSL and SQLCipher compile from source: a C compiler, `perl`,
  and `pkg-config` are required; no system copies needed.
- `wasm32-unknown-unknown` does not build clean (getrandom `js` feature gap);
  its CI job is advisory. The emscripten target
  (`wasm32-unknown-emscripten`) is configured in `.cargo/config.toml`.

## Conventions

- Commits: conventional-commit style with crate scopes, e.g.
  `fix(tdutils,tdclient): ...`.
- `cargo-deny` denies yanked crates and enforces a license allow-list; `tdtl`
  is the single GPL exception (build-time tooling, never linked into
  artifacts).
- Perf regression gate on master: `.github/perf-baseline.json` fails on
  build/test CPU time > +30%. Bench gate: ≤ 2× C++ per row
  (`bench/README.md`).
- Release/parity-audit runbook: `RELEASE.md`; harness details:
  `conformance/README.md`.

## Agent skills

### Issue tracker

Issues live in td-rs/td GitHub Issues, worked via the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Default five canonical triage labels (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`) recorded as `Status:` lines in each issue file. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: root `GLOSSARY.md` and `docs/adr/`. See `docs/agents/domain.md`.

