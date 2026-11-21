# The SecretChat state machine lands de-actorized in tde2e

The C++ SecretChatActor is one actor per chat with its own mailbox,
AlarmManager timers, and a context (binlog, net, Td refs). This tree
already runs a mixed concurrency model: real tdactor actors where async
DB work demanded them (message_db, dialog_db, the client pump) and plain
manager shapes everywhere else — a divergence documented in td.rs. The
SecretChat port takes the plain shape: a per-chat state machine hosted
in tde2e behind a trait, driven by the manager that owns net answers,
binlog save callbacks, and timer slots.

The C++ vocabulary maps rather than translates: `loop()` after a pfs
state save (eaa3f32fb) becomes the flush step keyed on the
save-completion callback; per-chat actors become per-chat state routed
by chat id; actor timeouts become manager timer slots. The
`[blocked: tde2e]` seam name stays honest — the state machine lives in
tde2e; the manager wiring stays in tdclient.

Considered options: a literal tdactor actor per chat (rejected: heavier
than the demonstrated need — nothing else in the secret-chat flows wants
a mailbox, and the tree's divergence pattern keeps managers owning the
pumps); hosting the state machine in tdclient beside the manager
(rejected: contradicts the seam vocabulary every parked site cites).
