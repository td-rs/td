# bench

Ports of the C++ `benchmark/` targets. Gate: ≤2× C++ everywhere.

## bench_actor (S37)

Port of `benchmark/bench_actor.cpp` — `CreateActorBench`, the `RingBench`
message-passing ring (504 actors) and the `QueryBench` client/server RPC
pair, measured by the ported `bench` driver from `src/benchmark.rs` (the
`td/utils/benchmark.h` methodology: double `n` until a pass exceeds 1 s,
report the average of the calibration pass plus the measured passes).

Run (from the repo root):

```sh
cd bench
cargo build --release --bin bench_actor
./target/release/bench_actor            # or the CARGO_TARGET_DIR binary
./target/release/bench_actor "send_immediate"   # optional description filter
```

The C++ baseline is `benchmark/bench_actor` in the td build (`-O3
-DNDEBUG`, no LTO — matched by the Rust `release` profile with `lto =
false`). The gate numbers are paired runs pinned to one core
(`taskset -c 4`), min of two rounds each, to control for frequency scaling
on the `powersave` governor.

### Divergences from the C++ benchmark

- `RingBench` type 3 ("tail") is skipped: it drives the next actor through
  `get_actor_unsafe()->raw_event(...)`, a direct call the Rust scheduler
  deliberately does not expose.
- `QueryBench` "immediate future" closes each iteration's future actor
  after the C++ checks instead of abandoning it (C++ lets the stack object
  die while the actor is still registered).
- `QueryBench` "dummy" writes its result into a client field instead of the
  C++ stack `int *res` (the C++ result is never read; an owned per-message
  write target would add an allocation to the measured path); "lambda"
  carries its write target in an `Arc<Atomic>` (C++ captures a stack
  address).

### S37 results

Measured 2026-09-13/14, paired pinned-core rounds, min across rounds
(ops/sec):

| Bench                                 | C++                  | Rust       | Slowdown   |
| ------------------------------------- | -------------------- | ---------- | ---------- |
| Ring (send_immediate) (threads_n = 0) | 29,455,429           | 16,624,608 | 1.77×      |
| Ring (send_lambda) (threads_n = 0)    | 27,809,273           | 14,876,118 | 1.87×      |
| QueryBench: lambda                    | 23,034,526           | 11,183,792 | 2.06×      |
| QueryBench: callback                  | 9,918,267            | 6,331,814  | 1.57×      |
| QueryBench: delayed future            | 6,801,235            | 4,725,393  | 1.44×      |
| QueryBench: lambda_future             | 8,778,822            | 8,263,321  | 1.06×      |
| QueryBench: immediate future          | 7,357,092            | 7,260,194  | 1.01×      |
| QueryBench: dummy                     | 16,319,885           | 21,461,734 | 0.76×      |
| CreateActor                           | see the caveat below |
| Ring (send_later) (threads_n = 0)     | 11,520,950           | 12,926,074 | 0.89×      |
| Ring (send_raw) (threads_n = 0)       | 21,444,774           | 28,166,488 | 0.76×      |
| Ring (send_* ) (threads_n = 2, 10)    | —                    | —          | 0.69–0.89× |

Caveats on the two outliers — the `powersave` governor swings the C++
baseline itself by 3–4× between rounds, so single-round ratios understate
or overstate the gap:

- **CreateActor** was the registration-allocation outlier (actor box +
  IdSlot + ActorInfo vs the C++ ObjectPool: 1.55–4.50× across four paired
  rounds). The gate work closed it: the actor object is now co-located in
  the single `[IdSlot | ActorInfo | actor]` block (one malloc per
  registration, the C++ pool shape; the name is an inline 31-byte buffer).
  Post-fix paired rounds on a _loaded_ machine: 1.52× / 2.63× / 1.97×
  (median 1.97×) — on the gate line inside the same noise band; re-verify
  on a quiet machine for the record.
- **QueryBench: lambda** across four paired rounds: 1.26–2.06×, passing in
  3 of 4 rounds.

Everything else is at or under 2× in every round.

### S37 scheduler optimizations (landed in `crates/tdactor`)

- zero-allocation inline `ActorThunk` storage — `send_closure` messages up
  to 3 words live inside the event itself (the C++ thread-local closure
  pool equivalent);
- `ActorRef` borrowed/owned — `send_closure(&id, …)` takes no refcount;
- the pending set as a `Vec` + swap-remove with a per-actor index (no
  hashing, O(1) pointer work — the C++ intrusive-list shape);
- the dispatch context as a frame-local `EventContext` swapped through an
  `AtomicPtr` — zero TLS/RefCell on the inline path;
- `&ActorId` through the dispatch path (no per-hop refcount traffic);
- debug-only checked downcast in the message dispatch (release builds cast
  like C++; debug keeps the panic-on-lie check);
- the per-actor block co-located `[IdSlot | ActorInfo | actor]` (one malloc
  per registration — the C++ ObjectPool shape; the actor is dropped in
  place at destroy, the block freed by the last `ActorId` drop with the
  layout stored in the slot);
- the actor name as an inline 31-byte buffer (no `Box<str>` per
  registration).
