# DialogParticipantStatus C++ oracle (M.5.3)

Pins the byte format and predicate matrix of `td::DialogParticipantStatus`
(DialogParticipant.h:360) by compiling td's own sources and dumping:

- `name:<hex>` — the record body stored through td's `td::store(uint64)` /
  TL-string framing with the log-event writer-version prefix (`Version::Next - 1`
  = 61, what `LogEventStorerUnsafe` writes);
- `rt:<name>:<hex>` — the same bytes after a store → parse → re-store cycle
  (self-consistency; every `rt:` line must equal its `name:` line);
- `pred:<name>:<digit matrix>:until=…:anon=…:rank=…` — the predicate matrix
  (order pinned by `matrix()` in probe.cpp);
- `effective:<hex>` — a restricted-member sample with the media rights set;
- `legacy_member:<hex>` + its `pred:` row — a version-30 (`<`
  `MakeParticipantFlags64Bit` = 46) buffer whose flag word is a legacy
  `uint32`, reparsed and re-stored in the modern framing.

The Rust `DialogParticipantStatus` (crates/tdclient/src/chat_manager.rs)
pins all of these lines in its M.5.3 tests. Regenerate with:

    ./build.sh [td-repo-root]

The oracle compiles `td/telegram/DialogParticipant.cpp` + `misc.cpp` fresh
against a pristine `tdutils`+`tdnet` tree extracted from the td repo's git
HEAD (the shared worktree may carry unrelated edits), the gitignored
generated tl headers in `td/generate/auto`, and the local config.h shadow
(openssl on for the misc.cpp md5 helper). The store/parse of
DialogParticipantStatus never touches td_api/telegram_api symbols, so the
generated fragments need not be compiled; `--gc-sections` drops the record's
object-builder functions.

Note: at `-O1` an unrelated g++ codegen quirk shifted the banned-until sample's
flag word by one byte; `-O2` output is clean and fully self-consistent (the
pinned bytes), so this script compiles at `-O2`.
