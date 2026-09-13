# Codegen baseline (S17)

Numbers for the tl-codegen generated modules — the sanity budget for the
Track B→I generator. Re-measure with:

```sh
cargo run --release -p tl-codegen --bin codegen_bench --locked
```

## Baseline (2026-09-13)

Reference machine: the dev box the S06–S17 steps ran on (rustc 1.95.0,
NixOS x86_64). Wall times are single-run in-process measurements; treat
±30% as noise.

| Scheme       | Time         | Output size  | Output lines |
| ------------ | ------------ | ------------ | ------------ |
| td_api       | 0.043 s      | 15,946,527 B | 427,112      |
| telegram_api | 0.031 s      | 8,057,872 B  | 236,325      |
| secret_api   | 0.001 s      | 259,868 B    | 7,718        |
| mtproto_api  | 0.000 s      | 125,855 B    | 4,089        |
| e2e_api      | 0.000 s      | 116,914 B    | 3,627        |
| **total**    | **~0.075 s** | **~24.4 MB** | **678,871**  |

td_api output includes the S12 JSON methods, the S16 `Object` umbrella,
and the `#[cfg(test)]` sample synthesizers + round-trip tests; the wire
schemes include binary store/parse and the feature-gated replay corpus
dump.

## Context numbers (same machine)

- Cold workspace build (`cargo clean` → `cargo build --workspace --locked`):
  **145 s** (S14).
- Workspace tests: 5,916 across 27 suites (S16 state).
- Differential gates at this baseline: S12/S16 JSON oracle 37/37 lines
  byte-identical; S15 replay 5,282 samples, 1,344 byte-identical re-stores,
  0 failures.
- Known non-goal: the JSON dispatch frames are small (552 B) thanks to the
  per-variant `__json_new_*` helpers — keep it that way when touching the
  dispatch emission (inline arms measured a 141 MB frame, see S16).
