# S37 — actor-runtime performance gate

Status: **infrastructure complete, gate not yet green.** `bench_actor` is the
verbatim port pair (`benchmark/bench_actor.cpp` vs `bench/src/bench_actor.rs`);
`bench/actor-gate.sh` builds both, runs 3 passes per side, takes the best pass
per case (background load only ever slows a run), and fails when any case
drops below half the C++ ops/sec. This file records the first paired runs and
the remaining gap.

## Conditions

- Shared workstation; load average swung 7–35 during these runs (other
  agents' builds + desktop apps). All numbers are provisional; re-run the
  gate on a quiet machine before trusting any borderline verdict.
- C++: td @ d1085f9ce, `build-audit` (Release, `-O3 -DNDEBUG`), best of 3.
- Rust: this branch, `cargo build --release`, best of 3.

## Paired run (best-of-3, pre-finish-fix)

| case                                   | C++ ops/sec | Rust ops/sec | ratio |
|----------------------------------------|------------:|-------------:|------:|
| CreateActor                            |   3 254 610 |    1 833 331 |  1.78 |
| QueryBench: callback                   |   6 370 587 |    8 517 776 |  0.75 |
| QueryBench: delayed future             |   4 273 076 |    2 163 813 |  1.97 |
| QueryBench: dummy                      |  12 610 793 |   15 353 868 |  0.82 |
| QueryBench: immediate future           |   7 244 380 |    3 947 981 |  1.83 |
| QueryBench: lambda                     |  14 744 893 |   15 840 239 |  0.93 |
| QueryBench: lambda_future              |   5 237 406 |    3 806 961 |  1.38 |
| Ring (send_immediate) (threads_n = 0)  |  31 150 178 |   10 282 981 |  3.03 |
| Ring (send_lambda) (threads_n = 0)     |  33 783 094 |    9 830 115 |  3.44 |
| Ring (send_later) (threads_n = 0)      |  11 109 595 |    7 677 731 |  1.45 |
| Ring (send_raw) (threads_n = 0)        |  32 367 223 |   18 109 291 |  1.79 |
| Ring (send_tail) (threads_n = 0)       | 260 048 196 |   93 406 379 |  2.78 |
| Ring (send_later) (threads_n = 2/10)   |     262–289k |      4.9–5.5k | 45–55 (pre-fix) |

## Post-fix spot checks

- Threaded rings after the `ConcurrentScheduler::finish` wake-all fix:
  24–69k ops/sec under load-31 (was 4.9–5.5k). Isolated-probe estimate on a
  quiet machine: 3.3–7.7 µs/hop vs C++ 3.5–7 µs/hop — expect near parity.
- Single-thread closure rings unchanged (see gap 1).

## Remaining gaps

1. **Closure send paths, single-thread (3.0–3.4×).** Every
   `send_closure`-shaped hop Box-allocates an `Event::custom` thunk and
   downcasts it. The plan's fix is S33's per-actor message-enum dispatch
   (no heap closure, no downcast); the S34 closure subset was only ever the
   interim mechanism.
2. **Ring send_tail (2.8×).** Pure `get_actor_unsafe` + `raw_event` chain at
   10.7ns/hop vs C++ 3.8ns — the C++ raw-pointer chain is nearly free; every
   nanosecond of the alive-check/slot-deref path counts here.
3. **Threaded rings.** Dominated by cross-scheduler wake latency; the
   `finish` wake-all fix removed the 10 s/pass stall. Quiet-machine estimate
   is 2–4×; if still over gate after S33, the next lever is an eventfd-style
   wakeup instead of the tokio channel (matches td's eventfd path).

## Gate

`./actor-gate.sh` — exit 0 = every case within 2× of C++. Needs a quiet
machine for the final verdict.
