# td-rs vs C++ TDLib — Final Benchmark Results

**Date:** 2026-09-28 (S96 optimization pass; S97 addendum same day, **S98
"fastest or parity" pass 2026-09-29 — see its addendum and the Verdict for
the current state**) · **Rust:** working tree on `97541b3`
(scheduler intrusive lists + fast-path dispatch, OpenSSL bridges, wide CBC
decrypt, block pool; bench profile thin-LTO + `codegen-units = 1`), release ·
**C++:** `benchmark/` targets built Release/Ninja in `build-d1` (tree `2a26e0107`,
same TDLib 1.8.67 pin) ·
**Machine:** AMD Ryzen 5 7600X (powersave governor), Linux/NixOS ·
**Method:** paired alternating rounds, every run pinned to one core (`taskset -c 4`),
**4 rounds per side (40 runs total)**, per-row number = min across rounds per side
(the S79 gate convention; the disk-state `db` family also reports the median —
see its section). Ratio = C++ ÷ Rust on ops/s → **< 1.0 means Rust is
faster**; the gate is ≤ 2× (Rust at most 2× slower).

> **S97 addendum (2026-09-28, later the same day — the dispatch/crypto pass).**
> A second pass closed five of the seven remaining Rust-slower rows to parity
> or better and moved the whole family forward; measured as **interleaved
> before/after A/B rounds of the two Rust builds** (4 rounds per side,
> alternating, min of rounds — the C++ side was not rebuilt, so the C++
> columns reuse the S96 reference numbers above). Changes: the scheduler's
> dispatch-context pointer became a plain `Cell` (the C++ plain-member
> `EventGuard` swap — the `AtomicPtr` was a locked xchg on every hop), the
> destroy path stopped paying clone/drop refcount round-trips, the
> double-free detector moved to debug/loom builds, `aes_cbc_encrypt` and
> IGE-encrypt route through the vendored OpenSSL EVP (`EVP_aes_256_cbc`,
> padding off — the exact C++ backend; IGE as the C++ 31-block-chunk CBC
> transformation; emscripten keeps pure-Rust single-pass forms), and the
> bench harness dropped two divergences that charged Rust-only costs
> (per-iteration `Arc` in QueryBench-lambda; `Arc` counter in CreateActor)
> plus the mt19937 twist became the canonical libstdc++ split-loop.
>
> | Row (A/B, min of 4 rounds) | before | after | Δ | vs C++ |
> | --- | ---: | ---: | ---: | ---: |
> | AES CBC Encrypt [8KB] | 130,027 | **176,717** | +35.9% | **1.01×** |
> | AES IGE encrypt [8KB] | 123,899 | **156,216** | +26.1% | **1.03×** |
> | mt19937_rand | 571,585,530 | **1,038,991,641** | +81.8% | **0.67×** |
> | QueryBench: lambda | 12,102,570 | **28,999,297** | +139.6% | **0.83×** |
> | Ring send_immediate (t0) | 23,538,554 | **37,971,661** | +61.3% | 1.37× |
> | CreateActor | 2,743,688 | **3,580,635** | +30.5% | 1.44× |
> | Queue(1) *(untouched control)* | 189,736 | 186,532 | −1.7% | 1.27× |
> |
> | Guard rows (must not regress) | | | | |
> | Ring imm t2 / t10 | 506,943 / 259,118 | 510,432 / 259,666 | +0.7 / +0.2% | 0.74× / 0.84× |
> | Ring send_lambda (t0) | 17,707,568 | **73,380,388** | +314% | 0.63× |
> | Ring send_later (t0) | 14,847,140 | 17,989,258 | +21.2% | 0.66× |
> | QueryBench lambda_future / callback / dummy | 10.1M / 11.6M / 28.0M | 12.9M / 15.6M / 25.7M | +29 / +35 / −8% | 0.69 / 0.64 / 0.96× |
> | AES IGE decrypt [8KB] | 127,075 | 126,914 | −0.1% | 0.87× |
> | Queue(10) | 1,862,134 | 1,878,175 | +0.9% | 0.41× |
>
> Still open after S97: `QueryBench: immediate future` (3.17× — the
> architectural promise row, unchanged by design), `CreateActor` (1.44× —
> the residual is the per-actor `VecDeque` mailbox buffer plus the tokio
> `run_main` pump around each batch; the locked-RMW inventory is gone),
> `Ring send_immediate` (1.37× — the remaining gap is the safe-Rust send
> checks + `RefCell` borrows C++ pays nothing for), `Queue(1)` (1.27× —
> crossbeam condvar handoff vs eventfd; within gate, no hot consumer).
> Gates re-run green with the pass: fmt; clippy `-D warnings` (tdactor,
> tdutils, bench — tdtl/tde2e still fail on this newer clippy for
> pre-existing reasons, one in generated code); workspace tests
> **8,284/0** (needs `RUST_MIN_STACK=16MiB` on this toolchain — the debug
> close-machine frames overflow the 2 MiB default also on the unmodified
> baseline); the tdactor loom model; the 14-symbol tdjson ABI gate
> (exactly 14); the S73/S74 offline conformance corpus **40/40
> byte-identical**; the `wasm32-unknown-emscripten` tdutils cross-check.

> **S98 addendum (2026-09-29 — the "fastest or parity" pass).** The bar
> moved from "within 2×" to "fastest or parity on every row", and this pass
> closed every remaining Rust-slower family except three bounded residuals
> (all ≤ 1.45×). Measured as a fresh full paired run (same methodology,
> 4 rounds per side, min and median), 2026-09-29, same machine; the
> absolute numbers of *both* sides run below the 09-28 session (the C++
> side was not rebuilt — same binary, same pinning; C++ CreateActor
> 5.14M→4.44M, C++ QueryBench immediate future 12.41M→9.54M on the min),
> so the ratios below compare same-day pairs only.
>
> The pass, in landing order:
>
> - **Send hot/cold split** (`tdactor/scheduler.rs`): the send checks
>   (`send_message_immediately` / `send_immediately_impl`) now inline at
>   every call site with every rare branch (mailbox backlog, mid-migration,
>   cross-scheduler) pushed behind `#[inline(never)]` cold halves — the
>   same split as C++ `Scheduler.h` (inline checks) vs `Scheduler.cpp`
>   (slow paths). Callgrind + disassembly showed the monolithic form fell
>   out-of-line for the 24-byte message-enum closures (a call frame +
>   register round-trip per hop) while small hand lambdas still inlined —
>   the entire send_immediate-vs-send_lambda 2× gap.
> - **The pools became real** (`tdactor/actor_id.rs`): the S96 block pool
>   had a 256-block retention cap that collapsed to a ~0% hit rate under
>   the benches' batch shape (create *n*, destroy *n* in one pump — an
>   LD_PRELOAD malloc-counter probe measured 2 global mallocs per actor
>   cycle). The C++ `ObjectPool` freelist is unbounded (ObjectPool.h:
>   plain lock-free stack, no cap), so parity means unbounded retention
>   here too — now the documented deliberate policy, matching the C++
>   memory profile. Oversized send-closure thunks and the promise
>   machinery's future actors (`FutureBox`, replacing `Box<FutureActor>`)
>   joined the pool, and actor mailboxes recycle their `VecDeque` buffers
>   (the C++ `ActorInfo` mailbox is a `vector<Event>` whose buffer dies at
>   pool re-init, so C++ pays that malloc per actor — any hit here is
>   ahead).
> - **Crypto backend parity** (`tdutils/crypto.rs`): `aes_cbc_decrypt`
>   through one bulk `EVP_DecryptUpdate` (the exact C++ backend); `sha512`
>   through the C++ thread-local `EVP_MD_CTX` pair (`make_digest` shape)
>   and `hmac_sha512` through its `EVP_MAC` form (SHA-256/SHA-1 stay
>   RustCrypto — those rows were already Rust-faster); and the IGE fix
>   that flipped the 64-byte rows from 1.8× *under* C++ to ~2× *over* it:
>   a decrypt `AesIgeState` no longer creates and keys a CBC-encrypt EVP
>   context its RustCrypto decrypt path never touches (the C++ decrypt
>   state initializes an ECB-decrypt `Evp` — its own backend — so the
>   port's eager CBC setup was pure waste), and the RustCrypto key is no
>   longer expanded twice per one-shot state.
>
> | Row (2026-09-29 paired, min of 4 / median) | C++ | Rust | ratio min · med |
> | --- | ---: | ---: | ---: |
> | AES IGE c-fn [64B] | 4,076,545 | **8,405,824** | **0.48 · 0.66** |
> | AES IGE state [64B] | 4,514,416 | **8,793,481** | **0.51 · 0.57** |
> | AES CBC Decrypt [8KB] | 1,066,687 | 1,070,397 | **1.00** · 1.08 |
> | AES CBC Encrypt [8KB] | 168,774 | **175,610** | **0.96** · 1.01 |
> | AES IGE encrypt [8KB] | 155,544 | 155,448 | 1.00 · 1.00 |
> | SHA512 [64B] | 4,369,006 | **6,169,589** | **0.71** · 0.86 |
> | HMAC-SHA512 [64B] | 1,040,217 | **1,700,932** | **0.61** · 0.91 |
> | Ring (send_immediate) t0 | 45,380,872 | 40,116,090 | 1.13 · **1.01** |
> | Ring (send_raw) t0 | 30,864,716 | **31,773,790** | **0.97** · 0.94 |
> | QueryBench: immediate future | 9,538,354 | 6,665,643 | **1.43** · 1.60 |
> | QueryBench: lambda | 19,350,411 | **22,484,036** | **0.86** · 0.86 |
> | QueryBench: delayed future | 4,906,038 | **5,230,874** | **0.94** · 0.94 |
> | CreateActor | 4,436,338 | 3,255,662 | 1.36 · 1.45 |
> | Queue (1) | 233,493 | 177,590 | 1.31 · 1.31 |
>
> **Every one of the 49 comparable rows is now within the 2× gate**; the
> crypto family is 21/21 at parity or Rust-faster (worst row 1.00–1.08),
> the actor family's worst row is `QueryBench: immediate future` at
> 1.43/1.60 (down from 3.37 — the residual is the per-iteration
> registration + destroy round-trip over pooled blocks, still ~60% more
> work than the C++ stack pair), then CreateActor 1.36/1.45 (the
> tokio-channel-mediated registration pump, amortized per batch) and
> Queue(1) 1.31 (crossbeam futex handoff vs eventfd — the transport
> choice, documented). Guards all held or improved: the multi-thread
> rings 0.84–0.86, send_lambda 0.51, send_later 0.68, callback 0.56,
> lambda_future 0.59, dummy 0.82, pbkdf2 0.71, mt19937 0.52, td_rand_fast
> 0.50, ssl_rand 0.80–0.86, SHA1/SHA256 0.56–0.59, HMAC-SHA256 0.51,
> CRC32 0.07, ECB 0.85, CTR 0.95, handshake 0.99/1.00, SqliteKV mins
> 0.99–1.04, Binlog min 0.17, async 0.68, SeqKv 0.50.
>
> A measurement note for the record: the 09-28 IGE-64B Rust numbers
> (5.09M/4.71M) did not reproduce on 09-29 even on the unchanged HEAD
> tree (2.4M) while every other row reproduced — both sides' absolute
> numbers drift between sessions on this box, which is why every ratio in
> this file compares same-session pairs. The S98 EVP-skip makes the
> question moot: the rows now run 8.4–9.4M against C++'s 4.5–5.9M.
>
> Gates re-run green with the pass: fmt; clippy `-D warnings` (tdactor,
> tdutils, bench); workspace tests **8,284/0** plus doctests (`RUST_MIN_STACK=16777216`); the tdactor loom model (2/2); the 14-symbol tdjson
> ABI gate (exactly 14 — the new EVP_MD/EVP_MAC linkage does not leak
> into the C ABI); the S73/S74 offline conformance corpus **40/40
> byte-identical** through the changed stack; the `wasm32-unknown-emscripten`
> tdutils cross-check (the emscripten-gated RustCrypto paths compile). The
> live test-DC gates still need live credentials; the mtproto handshake
> oracle still needs the original build dir's include plumbing (same
> limitation as 09-28).

## Verdict

**Current state (S98, 2026-09-29): all 49 comparable rows are within the
2× gate — most at parity or Rust-faster — and no family is systematically
behind. Crypto is 21/21 at ≤ 1.00–1.08 (the IGE 64-byte rows ~2× *faster*
than C++); the actor scheduler's worst row is `QueryBench: immediate
future` at 1.43 min / 1.60 median (down from 3.37×); the remaining
bounded residuals are CreateActor 1.36/1.45 and Queue(1) 1.31, both
understood (registration pump over the tokio channel; crossbeam-vs-eventfd
transport choice). The history below is the S96 state; see the S97 and
S98 addenda above for the passes that closed it.** All eleven over-2×
rows of the S96 morning run are closed:

| Row | morning | now | fix |
| --- | ---: | ---: | --- |
| Ring (send_immediate, t0) | 3.46 | **1.41** | intrusive lists + the C++ RunFunc/EventFunc split + fat dyn ptr + LTO |
| Ring (send_lambda, t0) | 3.08 | **0.65** (Rust faster) | `send_lambda_immediately` — the C++ inline-lambda shape (no pre-boxing) |
| QueryBench: lambda | 2.22 | **1.36** | the `send_closure` fast path |
| QueryBench: immediate future | 3.37 | 3.37 | **not closed** — architectural (actor section) |
| CreateActor | 2.23 | **1.48** | intrusive lists (no O(n) ready retain) + fat dyn ptr + block pool |
| AES CTR [8KB] | 4.72 | **0.98** | EVP bridge to the vendored libcrypto (`EVP_aes_256_ctr`) |
| AES CBC Decrypt [8KB] | 2.23 | **1.32** | one wide `decrypt_blocks` pass + vectorizable XOR chain |
| ssl_rand_bytes | 4.73 | **0.95** | `RAND_bytes` + the C++ thread-local 512-byte buffer, verbatim |
| Handshake (2048-bit DH) | 3.10 | **0.97** (Rust faster) | `BN_mod_exp` bridge — the exact C++ call, consttime Montgomery inside |
| SqliteKV (false/true) | 3.15 / 2.24 | **1.02 / 1.00** | **no code change** — the morning gap was one collapsed Rust round under min-of-rounds; the pragma sets were already identical |
| AES ECB [8KB] | 0.89 → 2.05* | **0.89** | *a thin-LTO codegen artifact with 16 codegen units; `codegen-units = 1` restores it |

Bonus rows that crossed to Rust-faster with the same pass:
`td_rand_fast` 1.77 → **0.64**, `ssl_rand_int32` 1.43 → **0.90**,
`QueryBench: callback` 1.10 → **0.65**, `Ring (send_later)` 1.02 → **0.71**.
The multi-thread rings (threads_n = 2/10) stayed Rust-faster throughout
(0.73–0.88) — the regression guard held across every scheduler change.

## bench_crypto — AES / hashes / RNG (21 shared rows)

| Row | C++ (ops/s) | Rust (ops/s) | ratio | gate |
| --- | ---: | ---: | ---: | --- |
| AES CTR [8KB] | 1,400,553 | 1,430,378 | **0.98** ✓ | EVP bridge |
| AES CBC Decrypt [8KB] | 1,136,422 | 863,815 | **1.32** ✓ | wide pass |
| AES CBC Encrypt [8KB] | 178,686 | 124,228 | 1.44 ✓ | |
| AES IGE state [64B] | 5,444,617 | 5,093,421 | 1.07 ✓ | |
| AES IGE c-fn [64B] | 5,952,157 | 4,706,792 | 1.26 ✓ | |
| AES IGE encrypt [8KB] | 160,248 | 121,756 | 1.32 ✓ | |
| AES IGE decrypt [8KB] | 109,832 | 120,756 | **0.91** ✓ | |
| AES ECB [8KB] | 149,911 | 168,452 | **0.89** ✓ | |
| pbkdf2 | 8,944,920 | 11,578,035 | **0.77** ✓ | |
| mt19937_rand | 699,059,610 | 528,810,658 | 1.32 ✓ | |
| td_rand_fast32 | 381,859,253 | 449,343,769 | **0.85** ✓ | |
| td_rand_fast | 283,396,891 | 444,502,380 | **0.64** ✓ | |
| ssl_rand_int32 | 36,483,443 | 40,573,985 | **0.90** ✓ | RAND_bytes bridge |
| ssl_rand_bytes | 1,084,365,024 | 1,136,692,753 | **0.95** ✓ | RAND_bytes bridge |
| SHA1 [64B] | 11,097,778 | 20,794,254 | **0.53** ✓ | |
| SHA256 [64B] | 10,610,958 | 18,162,557 | **0.58** ✓ | |
| SHA512 [64B] | 5,778,161 | 5,196,931 | 1.11 ✓ | |
| HMAC-SHA256 [64B] | 4,131,434 | 5,687,145 | **0.73** ✓ | |
| HMAC-SHA512 [64B] | 1,570,532 | 1,348,913 | 1.16 ✓ | |
| CRC32 zlib [8KB] | 627,494 | 7,480,548 | **0.08** ✓ | Rust 12× faster |
| CRC64 Anton [8KB] | 82,018 | 80,575 | 1.02 ✓ | |

Skipped (documented in S79): `AES CTR RAW OpenSSL`, `std_rand` — no Rust
counterpart.

## bench_handshake (1 row)

| Row | C++ | Rust | ratio | vs morning |
| --- | ---: | ---: | ---: | --- |
| Handshake (full 2048-bit DH pair) | 408 ops/s | 420 ops/s | **0.97** ✓ | 3.10 → Rust-faster |

Once per login per DC (auth-key creation), not on any message path. The
native `BigNum::mod_exp` now bridges to the vendored OpenSSL `BN_mod_exp`
— the exact C++ call; OpenSSL routes odd moduli to
`BN_mod_exp_mont_consttime` internally (the exponent is secret, the
consttime form is the point). The S59 known-answer vectors
(`dh_mod_exp_vectors`) and the 2,649-test tdmtproto suite are green.

## bench_db — KV stores (6 shared rows)

The disk-state family: this NVMe's throughput under concurrent load swings
whole rounds by 3–6× (both sides — round 4 collapsed on *both* this run),
so the table reports min and median. Min alone once turned a single
stalled Rust round into a "3.15× gap" (the morning numbers): the pragma
sets were already identical on both sides, and the clean rounds sit at
parity.

| Row | C++ min | Rust min | ratio (min) | Rust median | ratio (median) |
| --- | ---: | ---: | ---: | ---: | ---: |
| BinlogKeyValue\<ConcurrentBinlog\> | 147,408 | 1,004,123 | **0.15** ✓ | 1,010,578 | **0.81** ✓ |
| BinlogKeyValue is_encrypted(false) | 167,358 | 1,005,993 | **0.17** ✓ | 1,050,060 | **0.76** ✓ |
| SqliteKV is_encrypted(false) | 122,654 | 120,300 | **1.02** ✓ | 263,382 | **1.41** ✓ |
| SqliteKV is_encrypted(true) | 102,410 | 102,528 | **1.00** ✓ | 172,105 | **1.34** ✓ |
| SqliteKeyValueAsync | 11,891,093 | 17,680,271 | **0.67** ✓ | 18,360,567 | **0.65** ✓ |
| SeqKvBench | 18,100,491 | 45,078,015 | **0.40** ✓ | 49,495,522 | **0.37** ✓ |

## bench_actor — scheduler (18 shared rows)

| Row | C++ (ops/s) | Rust (ops/s) | ratio | gate |
| --- | ---: | ---: | ---: | --- |
| CreateActor | 5,143,111 | 3,465,751 | **1.48** ✓ | |
| Ring (send_lambda) (threads_n = 0) | 46,378,601 | 71,392,570 | **0.65** ✓ | Rust faster |
| Ring (send_later) (threads_n = 0) | 11,792,870 | 16,668,001 | **0.71** ✓ | Rust faster |
| Ring (send_immediate) (threads_n = 0) | 52,083,509 | 36,979,323 | **1.41** ✓ | |
| Ring (send_raw) (threads_n = 0) | 34,926,960 | 29,592,903 | **1.18** ✓ | |
| QueryBench: lambda_future | 8,938,488 | 12,765,049 | **0.70** ✓ | Rust faster |
| QueryBench: lambda | 24,157,082 | 17,823,671 | **1.36** ✓ | |
| QueryBench: dummy | 24,776,079 | 24,915,590 | **0.99** ✓ | parity |
| QueryBench: delayed future | 6,992,434 | 6,780,375 | **1.03** ✓ | |
| QueryBench: immediate future | 12,414,709 | 3,680,060 | **3.37** ✗ | documented below |
| QueryBench: callback | 10,044,802 | 15,491,844 | **0.65** ✓ | Rust faster |
| Ring (send_later) (threads_n = 10) | 220,505 | 259,308 | **0.85** ✓ | guard held |
| Ring (send_immediate) (threads_n = 10) | 218,546 | 247,714 | **0.88** ✓ | guard held |
| Ring (send_raw) (threads_n = 10) | 216,072 | 247,632 | **0.87** ✓ | guard held |
| Ring (send_later) (threads_n = 2) | 368,319 | 506,631 | **0.73** ✓ | guard held |
| Ring (send_immediate) (threads_n = 2) | 377,281 | 508,562 | **0.74** ✓ | guard held |
| Ring (send_raw) (threads_n = 2) | 379,690 | 507,276 | **0.75** ✓ | guard held |

**The one open row — `QueryBench: immediate future` (3.37, stable across
all four rounds).** Architectural, not scheduler: each iteration builds
and destroys a *registered* future actor plus boxed promise plumbing
(`Box<FutureActor>` + registration + hangup/destroy + the boxed message
thunks), where the C++ `PromiseActor`/`FutureActor` pair lives on the
stack and its `ObjectPool` release is a freelist pop. The row was 1.01×
at S79 and regressed to 3.37× *before* any S96 change; the block pool
improved it to ~3.1–3.3×. Closing it fully means a stack-shaped promise
redesign (self-referential actors), not a scheduler fix. It measures the
promise machinery in isolation — not on any transport path.

Skipped (documented in S37): `send_tail` (drives `raw_event` through
`get_actor_unsafe`, deliberately not exposed).

## bench_queue — mailbox transports (3 paired shapes)

| Queue shape (N) | C++ MpscPollableQueue | Rust crossbeam_channel | ratio |
| --- | ---: | ---: | ---: |
| Queue (1) | 237,703 | 176,589 | **1.35** ✓ |
| Queue (10) | 764,591 | 1,802,772 | **0.42** ✓ |
| Queue (100) | 13,661,681 | 14,377,910 | **0.95** ✓ |

Against the weaker C++ `InfBackoffQueue` baseline Rust is 8–10× faster at
every shape. The remaining C++ rows (Poll/Buffer/Fd/Pipe/Sem/Var queues)
have no Rust counterpart — the Rust actor mailboxes use crossbeam channels
exclusively.

## The S96 changes behind the numbers

- **tdactor** (`crates/tdactor/src/scheduler.rs`, `actor_id.rs`,
  `messages.rs`): intrusive doubly-linked ready/pending lists (the C++
  `ListNode` port — the `ActorInfo` *is* the node; the S37
  `Vec`+`swap_remove` scheme paid 72% of the ring's instructions and
  dirtied a foreign cache line per hop); the C++ `RunFuncT`/`EventFuncT`
  split for `send_closure`/`send_lambda_immediately` (the inline path
  runs the monomorphized closure directly — no `Event`, no thunk, the
  closure stays in registers); the fat `*mut dyn Actor` stored at
  registration (one load per dispatch instead of a fn-pointer
  trampoline); `mailbox_empty` as a `Cell<bool>` on the `is_running`
  line; a per-thread size-classed block pool (the `ObjectPool` release
  semantics). The scheduler now holds a single *registration reference*
  per actor (created at registration, released at destroy) — the C++
  pool-owns/weak-id contract — since the intrusive lists carry raw
  `ActorInfo` pointers.
- **tdutils/crypto.rs**: `AesCtrState` through `EVP_aes_256_ctr`
  (emscripten keeps the RustCrypto path — no OpenSSL there);
  `aes_cbc_decrypt` as one wide `decrypt_blocks` pass + XOR chain.
- **tdutils/bignum.rs**: `mod_exp` through `BN_mod_exp` on native.
- **tdutils/random.rs**: `secure_bytes` through `RAND_bytes` with the
  C++ thread-local 512-byte buffer, `add_seed` through `RAND_add` +
  generation bump, `secure_cleanup` zeroing the buffer. **Guarantee
  change, documented in the module:** secure bytes come from OpenSSL's
  fork-safe userspace DRBG (like tdlib) instead of per-call `getrandom`
  from the kernel.
- **bench profile:** thin LTO + `codegen-units = 1` — the C++ bench
  compiles the scheduler into the bench TU; cross-crate inlining is the
  Rust equivalent, and one codegen unit avoids a measured 2.2× LTO
  pessimization of the ECB loop (74K vs 162K ops/s with the default 16
  units — an inlining artifact; the same code is at parity without LTO).

## Gates re-run green with the changes

fmt clean; clippy `-D warnings` clean; workspace tests **8,274/0**; the
tdactor loom model; the tdjson 14-symbol ABI gate (exactly 14, nothing
extra — the OpenSSL linkage does not leak into the C ABI surface);
`requests_gate` AUDITS_GREEN; the S73/S74 offline conformance corpus
**40/40 records byte-identical** through the changed stack; tdutils
cross-checks for `wasm32-unknown-emscripten` (the emscripten-gated
RustCrypto/getrandom paths compile — a check that caught two real
compile errors in the gated code). The mtproto handshake oracle could
not run in this environment (its C++ recorder needs the
`compile_commands.json` include plumbing of the original build dir); its
coverage stands in via the DH known-answer vectors and the tdmtproto
suite. The live test-DC gates (S75/S76) need live credentials and were
not re-run.

## Reproduce

```sh
# C++ side (once): build the bench targets into the existing Release tree
nix-shell -p openssl openssl.dev zlib zlib.dev gperf ninja --run \
  'cmake --build ~/Projects/td/build-d1 --target bench_crypto bench_actor bench_db bench_handshake bench_queue -j8'

# Rust side (once):
cd ~/Projects/td-rs/bench && nix-shell -p rustup sccache gcc gnumake perl --run \
  'RUSTC_WRAPPER=sccache RUSTFLAGS="-C linker-features=-lld" cargo build --release'

# Paired rounds + table (scripts kept from the 09-28 runs):
bash /tmp/bench-compare.sh && python3 /tmp/bench-tabulate.py
```

Raw logs of this run: `/tmp/bench-compare-0928-1611/` (`cpp|rs-<bench>-r<1..4>.log`).
Rows follow the S79 skip list (OpenSSL-comparison rows, `std_rand`,
`send_tail`, `BinlogKeyValue<Binlog>`, encrypted-binlog creation,
`bench_http` — the last measured inside the gate at 1.36×, unchanged; the
binlog skips are pinned permanently in `bench/README.md`).
