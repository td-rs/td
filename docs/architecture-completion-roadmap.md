# Architecture completion roadmap (post-03220f6)

Phase: architecture completion, opened 2026-11-22 after the 23383aef →
03220f6 sync merged (#178) and the final tree gated green (`944d2dc`).
Decisions below were settled by design review (six + five + one
confirmation rounds) and are recorded here as the campaign contract;
ADR-0002 carries the SecretChat shape decision.

## Definition of Done

Every workstream landed or explicitly re-scoped with reasons, and every
one of the 13 outstanding seam-landing requirements either implemented
or re-recorded against the plane that actually landed. Census movement
and blocked-tag counts are evidence, not the goal.

## Gap summary (verified at 944d2dc)

- Request surface: 1,072/1,099 arms dispatched; the 27 sync arms are
  pure utilities.
- Blocked-seam heatmap: `[blocked: net]` 286 sites, `[blocked: tde2e]`
  16, `[blocked: files]` 16, `[blocked: binlog]` 4, `[blocked: database]`
  2. Milestone markers: M.10 ×563, M.12 ×179.
- Hosts already parity-verified: binlog (byte-identical records,
  replay processor, env-seam consumers), actor/scheduler (C++-mirror,
  in service for db + client pump), MTProto session stack, all five
  schemes pinned + generated, tde2e keychain subset, Files state model
  (26.6k lines with env-seamed transfers).

## Dependency graph

```text
                    net plane (286 tagged sites — separate commission)
                      │ gates live verification of:
                      ├──► upload executors ──► SR d2fcdc646 ──► 179 M.12 consumers
                      ├──► net-stats emission (f2fec5324)
                      └──► SecretChat live E2E (optional; offline paths close #175)

tdactor (ready) ─┐
binlog (ready)   ├──► SecretChat vertical ──► #175 four fixes ──► closes #175
pmc (ready)      │        ▲
secret_api (rdy) ┘        └── SC-1 codec env (first slice)

builders ──► SR gifts / audios / x.com / welcome-DB   (parallel track, no deps)
M.10 sub-planes ──► SR reply container / community fan-out / count plane
WS1 Action cluster: no edges — triage closure only
```

## Settled decisions

1. SecretChat first (tractability: zero architectural blockers, bounded
   ~3k-line C++ surface, offline-verifiable).
2. The four independent builder issues are a parallel track, not a phase.
3. The net plane is excluded from implementation; its gap assessment is
   commissioned separately and Phase-4 gating (upload) defers to it.
4. The Action/SimpleAction cluster is triage-closed absorbed/site-less.
   Tripwire: a demonstrated need (not parity) reopens
   QueryCombiner/SemaphoreActor ports.
5. Vocabulary: "SecretChat state machine" (the logic, in tde2e, behind a
   manager-driven trait) ≠ "actor" (strictly a tdactor::Actor). See
   GLOSSARY and ADR-0002.
6. #175 closes on offline evidence (byte-parity, replay, state-machine
   and env-fake ordering tests); live two-party E2E is a follow-up
   recording campaign. The issue stays open until all four fixes land.

## Issue breakdown (acceptance criteria live in the issues)

| Issue | Scope | Key acceptance |
|---|---|---|
| SC-1 | Production `SecretChatEnv`: decrypted-layer codec | byte-parity vs C++ `decryptedMessageLayer`; Inbound/Outbound records fully real |
| SC-2 | PFS/seq state machine core (de-actorized, tde2e) | `update_seq_no_state`/`run_pfs` parity; 5010b6120 conditions TDD'd |
| SC-3 | Binlog persistence + replay over real pmc | 3d03904cf write site; replay restores state |
| SC-4 | Net flows + flush/cancel ordering | eaa3f32fb + d30872700; closes #175 |
| SR-1..SR-4 | gifts / audios / x.com / welcome-DB | per-commit C++ parity tests |
| M10-1..M10-3 | reply container / community fan-out / count plane | per-commit parity; 093280641 Unread* filters |
| WS1 | Action cluster triage | per-commit verdict table with reasons |

Sequencing: SC-1 → SC-2 → SC-3 → SC-4 (each independently gateable);
everything else is parallel-track. Upload re-scopes behind the net
assessment; its fold point is d2fcdc646.
