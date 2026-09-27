# Vendored third-party forks

Patched upstream crates consumed through `[patch.crates-io]` in the workspace
root. Each fork documents its provenance here; keep the carried delta minimal
and emscripten/target-gated so native builds stay byte-identical.

## libsqlite3-sys 0.38.2

Upstream: <https://crates.io/crates/libsqlite3-sys/0.38.2> (rusqlite developers,
MIT license — see `libsqlite3-sys/LICENSE`).

**Carried delta:** exactly one hunk in `libsqlite3-sys/sqlcipher/sqlite3.c`
(~:109829), inserting an `__EMSCRIPTEN__` branch before the generic
`.fini_array` fini registration:

```c
#elif defined(__EMSCRIPTEN__)
    /* wasm: emcc's backend rejects .fini_array sections; module teardown runs
       no ELF destructors in the browser model, so skip the registration. */
    static void (*const sqlcipher_fini_func)(void) __attribute__((used)) = sqlcipher_fini;
#else
```

**Why:** emscripten's linker backend rejects `.fini_array` sections, and the
sqlcipher amalgamation registers its `sqlcipher_fini` destructor there on the
generic (non-Windows, non-Apple) path — so any wasm32-unknown-emscripten build
linking sqlcipher fails at link time. Module teardown in the browser model runs
no ELF destructors, so skipping the registration is behavior-preserving there.

**Verify the delta** (against the pristine crate from the cargo cache):

```sh
tar xf ~/.cargo/registry/cache/*/libsqlite3-sys-0.38.2.crate -C /tmp/lsql-pristine --strip-components=0
diff -ru /tmp/lsql-pristine/libsqlite3-sys-0.38.2 vendor/libsqlite3-sys
```

Expected: only the hunk above (plus this file). Upstreaming the branch to
rusqlite/libsqlite3-sys is an optional follow-on; the fork keeps the wasm build
self-contained either way.
