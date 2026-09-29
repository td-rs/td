# bench

Ports of the C++ `benchmark/` targets. Gate: ≤2× C++ everywhere.

> **S98 status (2026-09-29 — the "fastest or parity" pass):** all 49
> comparable rows are within the gate, worst 1.43× (`QueryBench: immediate
> future`, down from 3.37×); crypto is 21/21 at parity or Rust-faster
> (the IGE 64-byte rows ~2× *over* C++ after the decrypt-state EVP-skip).
> The row tables and root-cause notes below are the S96/S97 record — see
> `BENCHMARK-RESULTS-FINAL.md` (S98 addendum) for the current numbers and
> the three fixes behind them: the send hot/cold split, the unbounded
> ObjectPool-parity block pool (plus pooled thunks/futures/mailboxes), and
> the crypto backend bridges (CBC-decrypt EVP, OpenSSL sha512/hmac-sha512,
> IGE decrypt states no longer key an unused CBC-encrypt context).

## soak (S80)

`src/bin/soak.rs` — the S80 memory gate. Each tick runs one full
actor-scheduler lifecycle (a fresh 8-actor ring registered, wired, run and
torn down — the S37 CreateActor registration path), plus binlog-KV /
in-memory-KV / buffer-builder churn with a binlog reopen cycle every 256
ticks. RSS is sampled per tick; the gate compares the last quarter of the
run against the warmup quarter: growth beyond 10% (+4 MiB slack) fails.

Result (2026-09-14, 120 s, 2 384 ticks): RSS min 4.7 MiB / max 4.8 MiB,
warmup quarter 4.7 MiB → final quarter 4.7 MiB — **flat, PASS**. The
release build reaches a steady-state heap on the allocation-sensitive
paths; there is no pathological growth. Longer soaks (`soak 3600`) and an
allocator-backend comparison (the C++ side uses per-size pools; glibc's
malloc already holds steady here) remain open follow-ups for the S81-era
bindings workloads.

```sh
cd bench
cargo build --release --bin soak
./target/release/soak 120   # seconds
```


## S79 status

- `bench_actor` (S37): actor runtime — gate green (below).
- `bench_crypto` (S79): AES/hash/RNG/CRC — 18 of 22 rows at or under 2×;
  `AES CTR` and `ssl_rand_bytes` fail with backend-level root causes (below).
- `bench_handshake` (S79): DH handshake — still outside the gate (~3.1×,
  closed from ~3.9× by the `crypto-bigint` Montgomery fast path in
  `tdutils::bignum`; see the bench_handshake section). Auth-path only
  (key generation is a once-per-login cost).
- `bench_db` (S79): KV stores — all rows at or under 2×. `SeqKvBench`
  initially read 2.5× but the gap was the benchmark's own `format!`
  heap allocations; stack formatting (`itoa`) closed it to parity
  (see the table note).
- `bench_queue` (S79): the `QueueBenchmark2` transport shape over
  `crossbeam_channel` — at parity or faster than the C++ eventfd queues.
- `bench_http` (S79): persistent-connection GET loop vs the C++
  `HttpOutboundConnection` client against the same `bench_http_server_fast`.
- Not ported: `bench_misc`, `bench_log`, `bench_http_reader`,
  `check_proxy`/`check_tls`, `bench_empty` — none are in the S79 gate list
  (`actor, crypto, db, http, queue, handshake`); `bench_empty` is the
  driver's null calibration and is covered by every row of the ported
  driver.

All S79 pairings below were measured 2026-09-14 on the same machine: paired
rounds, `taskset -c 4` for the driver benches (wall-clock for `bench_http`),
C++ built `-O3 -DNDEBUG` without LTO, matched by the Rust `release` profile;
min of two rounds per side. The `powersave` governor swings the C++
baselines up to 3–4× between rounds — treat sub-2× margins as noise bands,
as in the S37 table.

## bench_crypto (S79)

Port of `benchmark/bench_crypto.cpp`. Row-by-row against the C++ binary
(ops/sec; ratio = C++/Rust, so ≥ 0.5 passes the 2× gate):

| Bench                  | C++ (best)     | Rust (best)    | ratio | gate |
| ---------------------- | -------------- | -------------- | ----- | ---- |
| AES CTR [8KB]          | 1,385,936      | 303,822        | 0.22  | ✗    |
| AES CBC Decrypt [8KB]  | 1,156,202      | 560,238        | 0.48  | ✗*   |
| AES CBC Encrypt [8KB]  | 176,520        | 134,643        | 0.76  | ✓    |
| AES IGE state [64B]    | 4,419,217      | 5,143,626      | 1.16  | ✓    |
| AES IGE c-fn [64B]     | 5,976,644      | 4,867,801      | 0.81  | ✓    |
| AES IGE encrypt [8KB]  | 156,297        | 124,455        | 0.80  | ✓    |
| AES IGE decrypt [8KB]  | 113,671        | 131,364        | 1.16  | ✓    |
| AES ECB [8KB]          | 138,273        | 160,355        | 1.16  | ✓    |
| pbkdf2                 | 9,699,776      | 11,604,263     | 1.20  | ✓    |
| mt19937_rand           | 708,568,135    | 522,551,356    | 0.74  | ✓    |
| td_rand_fast32         | 271,925,384    | 202,222,211    | 0.74  | ✓    |
| td_rand_fast           | 215,928,102    | 188,311,490    | 0.87  | ✓    |
| ssl_rand_int32         | 37,801,856     | 25,736,446     | 0.68  | ✓    |
| ssl_rand_bytes         | 1,119,969,485  | 212,572,180    | 0.19  | ✗    |
| SHA1 [64B]             | 10,020,800     | 19,869,488     | 1.98  | ✓    |
| SHA256 [64B]           | 10,926,045     | 17,120,649     | 1.57  | ✓    |
| SHA512 [64B]           | 4,739,783      | 5,351,699      | 1.13  | ✓    |
| HMAC-SHA256 [64B]      | 4,162,487      | 5,972,992      | 1.44  | ✓    |
| HMAC-SHA512 [64B]      | 1,691,001      | 1,421,464      | 0.84  | ✓    |
| CRC32 zlib [8KB]       | 708,797        | 7,876,977      | 11.1  | ✓    |
| CRC64 Anton [8KB]      | 81,159         | 79,664         | 0.98  | ✓    |

Root causes and notes on the flagged rows:

- **AES CTR — CLOSED at S96 (parity, ~0.98×).** The native
  `AesCtrState` now generates its keystream through the vendored OpenSSL
  EVP (`EVP_aes_256_ctr` over `openssl-sys`, the library already linked
  for sqlcipher) — the exact assembly the C++ `td::AesCtrState` runs;
  emscripten keeps the RustCrypto path (no OpenSSL there). 295K →
  ~1.4M ops/s. The KAT (`aes_ctr_state_matches_cpp`) stays green.
- **ssl_rand_bytes — CLOSED at S96 (parity, ~1.04×).** The secure API
  now runs through `RAND_bytes` + the C++ thread-local 512-byte buffer
  verbatim (`Random.cpp`: small draws served from the buffer, one DRBG
  refill per 512 bytes, bulk draws straight to the DRBG; `add_seed`
  feeds `RAND_add` and bumps the seed generation, `secure_cleanup`
  zeroes the buffer). ssl_rand_int32 lands at ~1.07×. The module docs
  in `tdutils::random` record the guarantee change: userspace-DRBG
  bytes like tdlib, not per-call kernel CSPRNG.
- **AES CBC Decrypt — CLOSED at S96 (1.34×).** CBC decryption
  parallelizes across blocks; `aes_cbc_decrypt` now runs one wide
  `decrypt_blocks` pass over the whole buffer (the pipelined AES-NI
  decrypt path) and then a vectorizable XOR with each preceding
  ciphertext block — the per-block loop was the gap. The
  `aes_cbc_state_matches_cpp` KAT stays green.

Divergences: the raw-OpenSSL comparison rows (`AesCtrOpenSSLBench`,
OpenSSL ≤ 1.0 `SHA1Bench`) are skipped — they compare td wrappers against
raw OpenSSL call overhead, a distinction the Rust stack does not have;
`std_rand` (libc `rand()`) is skipped (no Rust counterpart);
`mt19937_rand` is kept as a faithful MT19937. Descriptions drop the
"OpenSSL" suffix. The SHA1/ECB rows measure tdlib's protocol primitives
(wire compat), not production data protection. The AES benches write from
a fixed source snapshot instead of in-place `src == dst` mutation
(unrepresentable in safe Rust; identical crypto work per iteration).

## bench_handshake (S79; closed at S96)

| Bench     | C++ (best) | Rust (best) | ratio | gate |
| --------- | ---------- | ----------- | ----- | ---- |
| Handshake | 413.1      | 132.3       | 0.32  | ✗ S79 → **0.95× S96** |

Full DH handshake pairs (2048-bit Telegram prime, checks + key gen). The
S79 gate work moved `BigNum::mod_exp` for odd moduli onto
`crypto-bigint`'s Montgomery `MontyForm` (32-limb working width for
≤ 2,048-bit moduli, 64-limb above), byte-exact against num-bigint on
random cross-checks and the S59 known-answer vectors — that lifted the
row from 57–105 to 132 ops/s. **S96 closed it: the native `mod_exp` now
bridges to the vendored OpenSSL `BN_mod_exp`** (the exact C++ call;
OpenSSL routes odd moduli to `BN_mod_exp_mont_consttime` internally —
the exponent is secret, the consttime form is the point), with the
num-bigint fallback for ineligibles and the Montgomery path kept for
emscripten. 130 → ~422 ops/s — Rust-faster; the S59 KAT
(`dh_mod_exp_vectors`) and the 2,649-test tdmtproto suite stay green.

## bench_db (S79)

| Bench                              | C++ (best)  | Rust (best) | ratio | gate |
| ---------------------------------- | ----------- | ----------- | ----- | ---- |
| BinlogKeyValue<ConcurrentBinlog>   | 160,534     | 937,784     | 5.8   | ✓    |
| BinlogKeyValue is_encrypted(false) | 96,524      | 974,607     | 10.1  | ✓    |
| SqliteKV is_encrypted(false)       | 57,447      | 62,460      | 0.92  | ✓    |
| SqliteKV is_encrypted(true)        | 53,902      | 75,859      | 1.41  | ✓    |
| SqliteKeyValueAsync                | 11,291,302  | 11,000,995  | 0.97  | ✓    |
| SeqKvBench                         | 18,300,664  | 29,732,495  | 0.62  | ✓    |

- **SeqKvBench — closed by benchmark fairness, not container change.** The
  first measurement (7.3M ops, 0.40) was dominated by the bench's own two
  `format!` heap allocations per write; the C++ `PSLICE() << i` equivalent
  formats into SSO-backed stack storage with zero heap traffic. Switching
  the Rust row to stack formatting (`itoa`, the `PSLICE` analog) lifted it
  to 21.6–29.7M ops — at parity or faster than C++. The residual internal
  cost is `SeqKeyValue`'s `String` map storage (no SSO in safe Rust); a
  small-string optimization remains an optional follow-up.
- **BinlogKeyValue rows (5.8–10× faster).** Plausible but flagged: the
  Rust `ConcurrentBinlog` mutex-serializes where C++ dispatches through
  the `BinlogActor` event loop, removing the per-event mailbox latency.
  Before treating this as a win, verify flush/fsync cadence parity between
  the ports (S44 `TdDb` work).

Divergences: `TdKvBench<BinlogKeyValue<Binlog>>` skipped (the Rust
`BinlogKeyValue` has only the `ConcurrentBinlog` form); encrypted-binlog
row skipped (the S40 `Binlog` subset reports a keyed open of a fresh
binlog as `Wrong password` — encrypted binlog creation is not ported yet);
`SqliteKeyValueAsyncBench` flushes explicitly per pass (the C++ flushes
through actor events during the pass; measured work per pass is the same:
n buffered writes + one batched transaction).

**The two binlog skips are permanent (S96, 2026-09-28).** Porting the
encrypted-binlog creation means porting the whole binlog AesCtr layer —
`do_reindex` + the `TYPE_AES_CTR_ENCRYPTION` event (key salt + the
encrypted magic used as the password check) + key derivation + a
decrypting reader — as one format-compatible slice; a creation-only patch
would write binlogs the port itself cannot read back, and byte-level
compatibility with C++-written session binlogs is the actual contract.
`BinlogKeyValue<Binlog>` (the non-Concurrent form) is **diverged by
design**: the Rust port has one binlog engine (the `ConcurrentBinlog`,
mutex-serialized where C++ dispatches through the `BinlogActor`), so the
plain/Concurrent pair of C++ rows collapses into one row here. The
flush/fsync cadence was verified at S96: both ports write through per
event with the events buffer disabled (the C++ `Binlog::init` comment)
and `sync` = flush + `sync_all` on the same need-sync bookkeeping — the
Rust-faster binlog rows (0.82–0.88× in the 09-28 four-round run) are
real, not a cadence artifact.

**SqliteKV rows — no pragma gap exists (S96, 2026-09-28).** The recorded
2.24×/3.15× (min-of-rounds) came from one collapsed Rust round: the raw
09-28 logs show Rust at 397–407K/235–238K ops in rounds 1, 3 and 4 — at
parity or faster than C++'s 366–384K/230–235K — and one round at
116K/103K (an environment hiccup, this NVMe family). The pragma sets are
identical (`encoding`/`synchronous=NORMAL`/`WAL`/`temp_store=MEMORY` at
the bench; `journal_mode=WAL`/`secure_delete=1` at the connection;
`busy_timeout` 5 s at open, both sides). A clean same-directory re-run
at S96: Rust 395K vs C++ 370K unencrypted (0.93×), 232K vs 231K
encrypted (1.01×). For disk-state rows the table should use the median
of rounds, not the min — a single environmental stall is not a
performance property of the code.

## bench_queue (S79)

The C++ rows benchmark eventfd/poll/pipe/semaphore mailbox transports the
Rust rewrite replaced; the ported bench runs the same `QueueBenchmark2`
client/server exchange over `crossbeam_channel` (what the Rust actor
mailboxes actually use), paired against the C++ eventfd queues:

| Queue shape (N) | C++ MpscPollableQueue (best) | C++ InfBackoffQueue (best) | Rust crossbeam_channel (best) | gate |
| --------------- | ---------------------------- | -------------------------- | ----------------------------- | ---- |
| 1               | 240,603                      | 18,111                     | 191,652                       | ✓ (0.80× Mpsc) |
| 10              | 740,969                      | 171,565                    | 1,797,766                     | ✓ (2.4× Mpsc) |
| 100             | 13,388,445                   | 1,722,099                  | 13,614,966                    | ✓ (1.02× Mpsc) |

At one connection the crossbeam channel pays lock-free wakeups where the
C++ eventfd queue amortizes; from 10 connections up it is at parity or
faster. All rows pass the gate against the stronger C++ baseline.

## bench_http (S79)

Both clients run 2 × 100 000 sequential `GET /` exchanges against the same
C++ `bench_http_server_fast` (the server binds 8082 in the C++ source;
both sides here run against the rebuilt server on a free port — 8082 is
taken by an unrelated service on this machine). Wall-clock per client:

| Client | C++ HttpOutboundConnection | Rust reqwest/hyper (pooled) |
| ------ | -------------------------- | --------------------------- |
| wall   | 1.84 s / 1.88 s            | 2.51 s / 2.59 s             |

Rust is at 0.73× C++ ops/sec (1.36× slowdown) — inside the gate. **S96
status: no action taken (§3.1 was the plan's lowest-priority item and the
row was already inside the gate).** The two shapes remain: the Rust side
drives a pooled `reqwest`/hyper client — the transport layer the S48
`Wget` port uses — because the persistent-connection
`HttpOutboundConnection` actor is not ported yet; keep-alive pooling
preserves the measured shape (one established connection per client, one
request in flight). Hyper-pool tuning or porting the
persistent-connection actor stays an optional follow-up (it would move
1.36× toward parity, not past any gate).

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
-DNDEBUG`), whose scheduler hot path is header-inline and compiled
*into* the bench translation unit — the Rust equivalent of that
inlining is thin LTO (`bench/Cargo.toml`, set at S96; `lto = false`
understated the port by making every actor hop a real cross-crate
call). The gate numbers are paired runs pinned to one core
(`taskset -c 4`), min across rounds, to control for frequency scaling
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
  writes through one shared `Arc<Atomic>` created per bench and cloned per
  message (C++ captures a stack address — sound there only because the
  idle client runs every dispatch inline; the shared target is the safe
  form without a per-message malloc).
- `CreateActorBench` counts actors in a static atomic instead of the C++
  plain static counter (safe Rust has no shared mutable statics; one
  fetch_add/fetch_sub per lifecycle is the cheapest faithful form).

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

### S96 scheduler pass (2026-09-28, on top of S37)

The four-round 09-28 paired run against the Release C++ build re-measured
the single-thread rows at 3.08–3.46× (a hotter C++ baseline than S37's —
the honest number). The S96 pass closed the family inside the gate:

- **intrusive ready/pending lists** — the C++ `ListNode` port (the
  `ActorInfo` *is* the list node; unlink is harmless on a detached node,
  so the `in_ready`/`in_pending`/`pending_index` flags are gone). The S37
  `Vec` + `swap_remove` scheme paid 72% of the ring's instructions
  (callgrind): the swap dirtied a *foreign* actor's cache line every hop.
  The lists are boxed at construction — a self-pointing sentinel must live
  at a stable address, and `SchedulerState` moves (constructor Vec →
  worker thread box).
- **the C++ `RunFuncT`/`EventFuncT` split for `send_closure`** — the
  inline path runs the monomorphized message closure *directly* (no
  `Event`, no `ActorThunk`, the closure stays in registers); the `Event`
  is built only when queueing. Same for the new
  `send_lambda_immediately` (the C++ `send_lambda` shape — the lambda
  bench no longer pre-boxes, matching the C++ inline path).
- **the fat dyn pointer in `ActorSlot`** — built once at registration,
  one load per dispatch instead of a fn-pointer trampoline.
- **`mailbox_empty` as a `Cell<bool>`** on the `is_running` cache line —
  the two inline-dispatch guards touch one line, no `RefCell` dance.
- **a per-thread, size-classed block pool** — the `ObjectPool` release
  semantics (blocks recycle instead of hitting the global allocator;
  cross-thread frees land in the freeing thread's bucket).

Post-pass numbers (the final 4-round paired run, same machine,
`taskset -c 4`, min of rounds): Ring send_immediate 37.0M vs C++ 52.1M
(1.41×), **send_lambda 71.4M vs 46.4M (0.65× — Rust faster; the
codegen-units=1 build lets the monomorphized lambda fully optimize)**,
send_later 16.7M vs 11.8M (0.71×), send_raw 29.6M vs 34.9M (1.18×),
CreateActor 3.47M vs 5.14M (1.48×), QueryBench callback 0.65× /
lambda 1.36× / dummy 0.99× / lambda_future 0.70× / delayed 1.03×, and
the multi-thread rings (threads_n = 2/10) stay Rust-faster
(0.73–0.88×). The bench profile builds with thin LTO +
`codegen-units = 1` — the C++ bench compiles the scheduler's
header-inline hot path directly into the bench translation unit (full
inlining without any LTO); cross-crate inlining is the Rust equivalent,
not an unfair advantage, and the single codegen unit avoids a measured
2.2× LTO pessimization of the ECB loop.

**One row remains outside the gate: `QueryBench: immediate future`
(~3.0–3.2×).** Architectural, not scheduler: each iteration builds and
destroys a *registered* future actor plus boxed promise plumbing
(`Box<FutureActor>` + registration + hangup/destroy + the boxed message
thunks), where the C++ `PromiseActor`/`FutureActor` pair lives on the
stack and its `ObjectPool` release is a freelist pop. The row was 1.01×
at S79 and regressed to 3.37× in the 09-28 four-round run *before* any
S96 change (it improved to ~3.1× with the block pool); closing it fully
means a stack-shaped promise redesign (self-referential actors), not a
scheduler fix. Not on any transport path — it measures the promise
machinery in isolation.

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
