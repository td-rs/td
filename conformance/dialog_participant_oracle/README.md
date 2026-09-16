# M.5.4 DialogParticipant oracle

Pins the Rust `DialogParticipant` port (`crates/tdclient/src/chat_manager.rs`,
M.5.4) against tdlib's own `DialogParticipant.cpp`/`DialogId.cpp`:

- `dp:`/`rt:` — record `store` bytes under the log-event version prefix
  (`Version::Next - 1` = 61) and the store→parse→re-store self-consistency;
- `conv:`/`fld:` — the nine `telegram_api::ChatParticipant` /
  `telegram_api::ChannelParticipant` constructor conversions (basic-group
  member/creator/admin, channel member/self/creator/admin/left/banned plus
  the view-messages-banned broadcast form), each dumped as stored bytes and
  raw fields;
- `leg:`/`fld:`/`rt:` — legacy parse vectors: version 30 (dialog as a bare
  `int32` user id, `uint32` status flag word), version 31 (dialog `long`,
  inviter still `int32` — the wrapping negative inviter is preserved as-is)
  and version 33 (inviter `long`).

The status tail of every vector rides the M.5.3 oracle
(`../chat_participant_status_oracle`); this probe pins only the framing
around it (the two id `long`s / version-gated `int32`s and the `int32` join
date).

`channelParticipantLeft` has no default constructor, so its vector is fed
through the generated `fetch()` as hand-stored TL words (ctor id +
`peerChannel`) — the same decode path a server message takes.

Usage: `./build.sh [td-repo-root]` — compiles probe.cpp + the real
DialogParticipant.cpp / DialogId.cpp / misc.cpp against a pristine
git-archive tdutils+tdnet tree (the shared td worktree may carry parallel
sessions' edits). The generated `telegram_api_*.cpp` TU set compiles at
`-O0` (only the participant/peer vtables survive `--gc-sections`); the logic
TUs stay at `-O2` — the M.5.3 probe caught `-O1` miscompiling the status
bytes. Requires `td/generate/auto` (see `../generate_td_auto`). Output lands
in `oracle.out` (gitignored); the Rust tests quote those hex lines.
