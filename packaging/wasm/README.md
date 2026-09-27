# S82 wasm runtime smoke — recipe + findings (2026-09-27)

The runtime smoke PASSED: the standalone tdjson module instantiated in Node
(via the emcc MODULARIZE glue) and `td_execute(getTextEntities)` returned the
byte-correct response (4 entities — mention, botCommand, url ×2, identical to
the S81 native cpp/python/java legs). `td_create_client_id` aborts on the
documented pthread wall: tdactor's scheduler spawn → `Os { code: 138,
Unsupported }` → non-unwinding panic (needs the nightly/-Zbuild-std
+atomics link).

## The working link recipe (the cdylib -shared mode is NOT usable)

1. tdjson built as `staticlib` (crate-type rlib+staticlib — the smoke's local
   worktree edit; cdylib must be dropped for the build because rustc's
   -shared link demands -fPIC C objects):
   `env CC=emcc CXX=em++ AR=emar RANLIB=emranlib rustup run 1.95.0 cargo
   build --locked --target wasm32-unknown-emscripten -p tdjson`
2. Hand-link with emcc (executable flavor — everything resolves, real
   standalone module + JS glue):
   `emcc libtdjson.a <sqlite3.o> -o tdjson.js --no-entry -sMODULARIZE=1
   -sEXPORT_NAME=tdjson -sALLOW_MEMORY_GROWTH=1 -sSTANDALONE_WASM=1
   -fwasm-exceptions -L<libz out>/lib -lz -sEXPORTED_FUNCTIONS=<the 14>+_malloc,_free
   -sEXPORTED_RUNTIME_METHODS=HEAPU8`
   → tdjson.js (171KB glue) + tdjson.wasm (28.7MB debug; imports = 26 WASI
   shims only, 14 td_* exports).

## Findings (the walls, precisely)

- rustc's cdylib link on this target uses emcc `-shared` →
  `--unresolved-symbols=import-dynamic`: the module IMPORTS its own statics
  (GOT) + every C symbol — 105 imports in release, 4,870 in debug — not
  runnable even with stubs. The release artifact
  (`target/wasm32-unknown-emscripten/release/tdjson.wasm`, 12.95MB, 14
  exports) links but has this shape; a RUNNABLE artifact needs the
  staticlib+emcc recipe.
- **cc-rs silently used the HOST gcc for -sys builds** (x86-64 ELF objects
  inside "wasm" archives — libz, libsqlcipher). The emcc toolchain env
  (CC/CXX/AR/RANLIB) must be forced; zlib then builds as wasm fine.
- **sqlcipher + vendored-openssl on emcc: backend error ".fini_array sections
  are unsupported"** (from the openssl headers' destructor attributes). The
  smoke used plain `rusqlite/bundled` (tddb feature override — LOCAL ONLY,
  never land: sqlcipher is the plan's decision). The sqlcipher-on-wasm
  question (different crypto provider — sqlcipher's own or a wasm-friendly
  one) is the follow-up.
- Plain bundled sqlite compiled clean with emcc (the PIC errors only affect
  the -shared cdylib leg, not the staticlib hand-link).
- The glue needs `-sEXPORTED_RUNTIME_METHODS=HEAPU8` for JS-side memory
  access; `_malloc`/`_free` in EXPORTED_FUNCTIONS for string marshalling.

Artifacts: /tmp/tdrs-s82-dist/{tdjson.js,tdjson.wasm,smoke.cjs};
worktree /tmp/tdrs-s82-rel (branch s82wasm = master b127332 + the two
smoke-only dirty Cargo.toml edits — do NOT commit as-is).

# PTHREAD LINK ACHIEVED (2026-09-27, same day) — the full Td startup runs in wasm

`PTHREAD_SMOKE_PASS`: with the nightly atomics build, `td_create_client_id`
returns 1 (the tdactor scheduler thread spawns), `td_send(getOption version)`
activates the client, and `td_receive` delivers BOTH `updateOption version`
AND `updateAuthorizationState → authorizationStateWaitTdlibParameters` — the
manager plane's startup state machine runs in WebAssembly (Node runtime),
the same milestone the S81 native cpp/java legs reached.

The recipe (flags-only, no source changes over master b127332):
1. `rustup toolchain install nightly --profile minimal --component rust-src`
   + `rustup target add wasm32-unknown-emscripten --toolchain nightly`
   (tested: 1.101.0-nightly 2026-09-26).
2. Build the staticlib with std rebuilt from source:
   `env CC=emcc CXX=em++ AR=emar RANLIB=emranlib CFLAGS=-pthread CXXFLAGS=-pthread
   RUSTFLAGS="-C target-feature=+atomics,+bulk-memory,+mutable-globals"
   cargo +nightly build -Z build-std=std,panic_abort --locked
   --target wasm32-unknown-emscripten -p tdjson`
   — **the CFLAGS=-pthread is REQUIRED**: RUSTFLAGS does not reach cc-rs, and
   without it the C objects lack atomics and wasm-ld rejects --shared-memory.
3. Hand-link with emcc (all the standalone flags plus):
   `-sUSE_PTHREADS=1 -sPTHREAD_POOL_SIZE=4
   -sDEFAULT_PTHREAD_STACK_SIZE=33554432 -Wl,-z,stack-size=33554432
   -sINITIAL_MEMORY=134217728`
   — **the 64KB default stacks trap** in td_send (the debug build's huge
   frames; the wasm twin of the native RUST_MIN_STACK=16MiB requirement).
4. Node smoke: `smoke-pthread.cjs` (create → send getOption → poll receive).

Artifacts: /tmp/tdrs-s82-dist/{tdjson-pthread.js,tdjson-pthread.wasm,
smoke-pthread.cjs}; build tree /tmp/tdrs-s82-rel/target-pthread.

# SQLCIPHER-ON-WASM SOLVED (2026-09-27, later) — the libtomcrypt provider

**Root-cause correction**: the `.fini_array` wall was NEVER the openssl
headers — it is sqlcipher's OWN fini registration (sqlite3.c ~:109830: the
non-Windows/non-Apple `#else` puts `sqlcipher_fini` in `.fini_array`; the
emscripten target defines neither, and emcc's backend rejects the section).
The openssl attribution died on bisection (no destructor attrs in the
preprocessed output; every individual header compiles clean).

**The provider**: the amalgamation carries `SQLCIPHER_CRYPTO_LIBTOMCRYPT`
all along — libtomcrypt 1.18.2 (fetched from the GitHub release) compiles
clean under `emcc -pthread` (412 objects, zero failures, 1MB
libtomcrypt.a), wired WITHOUT any openssl:
- tddb feature: `bundled-sqlcipher` (NOT vendored-openssl — build.rs
  proceeds fine with no openssl when none is found off-Windows);
- CFLAGS += `-DSQLCIPHER_CRYPTO_LIBTOMCRYPT -I<ltc>/src/headers`;
- the 6-line emscripten branch on the fini registration (a LOCAL registry
  patch for the experiment — `~/.cargo/registry/.../libsqlite3-sys-0.38.2/
  sqlcipher/sqlite3.c`; the LANDING path is a `[patch]` fork of
  libsqlite3-sys or upstreaming; the registry patch reverts on re-extract);
- libtomcrypt.a passed at the emcc hand-link.

**Runtime proofs**:
1. The codec test (emcc binary linking the same sqlcipher.o + libtomcrypt.a,
   `-sNODERAWFS -sUSE_PTHREADS`, run in Node):
   `SQLCIPHER_WASM bad_key_rejected=1 good_key_reads=1` — the wrong key
   fails page-1 decryption (the codec + libtomcrypt AES actively
   encrypting), the right key reads back. Encrypted write → wrong-key
   rejection → right-key read, in WebAssembly.
2. The Td path: `tdjson-sqlcipher.{js,wasm}` (the full module) — the Td
   startup creates `tdlib_data_wasm/{db.sqlite, db.sqlite-journal,
   td.binlog}` through the sqlcipher-linked sqlite (NODERAWFS node build;
   the standalone-WASI layer lacks mkdir — browsers get the real FS in the
   tdweb harness slice). Keyed opens stop at the recorded S41 seam (the
   binlog's Wrong-password band on keyed opens of unencrypted binlogs —
   unchanged native behavior, "empty key until S41"); the empty-key path
   runs the db fully.
3. The remaining startup silence after db-init (no WaitPhoneNumber in the
   window) is the deeper init on wasm (timers/net shims) — the WebSocket
   transport slice's domain, not crypto.

Artifacts: /tmp/tdrs-s82-dist/{libtomcrypt.a, tdjson-sqlcipher.{js,wasm},
smoke-sqlcipher.cjs, sqlcipher-test.c/.js}.

# TDWEB HARNESS DONE (2026-09-27, final) — TDWEB_SMOKE_PASS in a real browser

Chromium 153 (headless, driven via CDP) loading the 18.7MB
`tdjson-pthread-release.{js,wasm}` in a REAL browser Web Worker, from a
COOP/COEP-served origin — **PTHREAD_SMOKE_PASS reproduced in the browser**:
crossOriginIsolated=true on the page AND the worker, importScripts loads the
glue, the module instantiates, `td_create_client_id=1` (the tdactor
scheduler spawns as nested browser Web Workers — emcc 6's self-as-worker
pattern, no separate .worker.js file), and td_send+td_receive deliver
`updateOption version` + `authorizationStateWaitTdlibParameters`.

The harness (preserved here + /tmp/tdrs-s82-dist):
- `coop-server.py` — serves the artifacts with COOP/COEP (SAB requires
  cross-origin isolation) on 127.0.0.1:8907.
- `tdweb-harness.html` + `tdweb-smoke-worker.js` — the page spawns the
  worker; the worker importScripts the glue, runs the smoke (MEMFS — the
  default browser FS; OPFS durable storage is the tdweb product follow-on),
  postMessages progress; the page also mirrors messages to /report.
- `cdp-driver.cjs` — the driver (node ≥24's built-in WebSocket; the
  MCP browsers wanted /opt paths): navigate + poll the log element.
- Gotchas: headless `--dump-dom` fires at load-complete and kills async
  work (a slow-resource hold only DEFERS the worker spawn — don't); the
  sandbox reaps `nohup ... &` servers — run them in tmux; `pgrep -f` sees
  your own wrapper shell (bracket the pattern).

The wasm remainder is now ONE item: the WebSocket transport.
