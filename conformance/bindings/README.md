# S81 — bindings validation against the tdlib examples (D6)

*Track J hardening, 2026-09-23. Verified against the Rust `tdjson` cdylib
built from master on the gate box (192.168.0.30), release profile.*

The drop-in contract (D3/D6): the bindings tdlib itself ships at the pinned
commit — `example/`: `python`, `java`, `csharp`, `swift`, `uwp`, `cpp`,
`web` — must keep working against the Rust artifact set, and nothing ships
beyond them. `example/android` and `example/ios` are SDK build harnesses,
not bindings, and are out of S81 scope.

The matrix separates **runtime** validation (the binding's own code loaded
the Rust `.so` and drove it end to end) from **static** validation (the
binding's sources reference only symbols/artifacts the Rust set provides —
see `conformance/verify_symbols.sh`, S83: the cdylib exports exactly the
`tdclientjson_export_list` 14, nothing else).

| binding | consumes | validated | evidence |
|---|---|---|---|
| `example/python` | the 14-symbol C ABI via `ctypes` | **RUN green** | `python_smoke.py` drives the example's own `TdExample` class against the Rust cdylib — transcript below. Exit 0. |
| `example/swift` | C ABI via a module map over `td_json_client.h` | **static green** | the example's sources reference exactly `td_create_client_id` / `td_send` / `td_receive` — a strict subset of the exported 14. The run-level check needs Xcode/Apple — owner item on macOS. |
| `example/uwp` | C ABI via the generated Windows wrapper (`Telegram.Td`) | **static (platform-blocked)** | the wrapper consumes the C ABI exports (`tdjson.dll`); the toolchain is Windows-only (C++/CX, MSIX). |
| `example/csharp` | `Telegram.Td.dll` (generated .NET wrapper, Windows native libs) | **static (D4-blocked)** | the example is a thin client of the generated wrapper DLL; that regeneration is explicitly deferred by **D4**. It resolves the same C ABI exports, which the cdylib provides (S83). |
| `example/java` | JNI shim `td_jni.cpp` over the **C++ API** (`td/telegram/Client.h`, `td_api.h`, `tl/tl_jni_object.h`) | **out of C-ABI scope (D4)** | structurally a C++-API consumer — cannot link against a C-ABI-only artifact by design. D4 defers JNI regeneration; the drop-in contract (D3) is the 14-symbol JSON ABI. |
| `example/cpp` | the C++ API (`td/telegram/Client.h`) | **out of C-ABI scope** | same structural case as java. |
| `example/web` | emscripten-built `libtdjson` → `tdweb` NPM package | **→ S82 (wasm)** | see the S82 section of `RELEASE.md` for the wasm build status. |

## The python run

The example sources are **not modified and not vendored into this repo**;
the gate box holds a copy of `example/python` at the pinned commit under
`~/tdrs-gates/td-bindings-example/python`. The driver only redirects the
example's library lookup to the Rust cdylib:

```sh
scp <td-checkout>/example/python gate:~/tdrs-gates/td-bindings-example/
cargo build --release -p tdjson                      # -> target/release/libtdjson.so
python3 conformance/bindings/python_smoke.py \
    target/release/libtdjson.so ~/tdrs-gates/td-bindings-example/python
```

Transcript (2026-09-23, gate box, master + the two client fixes below):

```
== loading /home/sadiq/tdrs-gates/target-trackj/release/libtdjson.so through the example's ctypes binding ==
td_execute getOption version -> {"@type": "optionValueString", "value": "1.8.67"}
updateAuthorizationState(waitTdlibParameters) received
authorizationStateClosed received
PASS: tdlib python example runs unmodified against the Rust tdjson
```

(process exit 0 — the closed manager client and the leaked manager singleton
tear down cleanly, matching the C++.)

## Real defects the validation surfaced and fixed

1. **`td_create_client_id` did not create the Td eagerly.** The C++
   creates the Td actor inside `create_client_id` (Client.cpp carries the
   wish as "TODO: create Td actor here lazily"); the port made manager
   clients lazy, so a bare `td_receive` — the first call every shipped
   binding's receive loop makes — returned nothing, forever. The example
   hung at its first receive with 0 events. Fixed to the C++ shape
   (`create_client_id` creates the Td via the pool) with the regression
   test `fresh_client_id_emits_the_startup_authorization_state` in the
   tdjson crate.
2. **The close-notification erase could deadlock the whole client.**
   Erasing the closed client's record dropped the last `Arc<MultiImpl>`,
   running the scheduler shutdown + pump-thread join inline on the receive
   thread — racing the pump that was still delivering that very
   notification (1-in-N hang, gdb-confirmed all-thread stacks). Fixed to
   the C++ shape: the manager's `Impl` owns the `MultiImpl` for its
   lifetime; per-client records only reference it. Covered by
   `python_example_flow_bare_receive_then_close_then_legacy`.
3. **The S78 restart's first fuzz smoke of `message_content` found a real
   OOM** in the same tree: `FormattedText::parse` reserved its entity
   vector from the raw stored count (32 GB from a 17-byte blob —
   `oom-bf05eca0`). Fixed with the C++ `get_left_len()` gate; see
   `fuzz/audit-2026-09-23.md`.

## Known divergences (documented, not fixed here)

- **Close latency after a pre-auth config fetch.** `close` currently waits
  out the pending config fetch's natural network timeout (~100 s) instead
  of cancelling it; the C++ settles in <1 s. The smoke bounds the wait at
  180 s and passes. This is the net-close-drain theme (M.7.12–15/M.14 net
  integration), not a bindings-surface defect.
- **A legacy (`td_json_client_*`) receive after a manager close in the
  same process trips the concurrent-receive `LOG(FATAL)`**
  (`client.rs` "Receive is called after Client destroy..."). The shipped
  examples never mix the two APIs in one process; the pure legacy flow and
  the pure manager flow are each clean (the legacy trio is covered by the
  repo's own tests). Recorded for the same close-drain follow-up.

## Why "static green" is real evidence

Every shipping tdlib binding ultimately resolves the
`tdclientjson_export_list` surface audited in S83: if all 14 symbols are
present with the C signatures in `include/td/telegram/td_json_client.h`
(vendored byte-identical from the pinned commit), a binding whose sources
reference a subset of those symbols cannot fail to load. The runtime half
is proven once, on python (the only pure-ctypes binding that runs on
Linux); the platform-blocked halves reduce to the same symbol check plus
toolchains this repo's gate box does not carry.
