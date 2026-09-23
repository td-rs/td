#!/usr/bin/env python3
"""M.10.22 exhaustiveness audit, switch 2 — the M.7 fan-out census.

REMAINING.md pins the fan-out census: every `MessagesManager::on_update*`
overload in the C++ tree must have a live sink, a recorded seam, or a
documented drop — never a silent trace drop. This script:

  1. extracts the `void MessagesManager::on_update*(...)` overload set from
     the C++ tree ($TD_CPP/td/telegram/MessagesManager.cpp, default
     ../td relative to the repo root);
  2. checks every overload has a row in `m7_fanout_status.txt` (this
     directory) with status `live`, `seam` or `drop`;
  3. checks every `live` row's evidence anchor actually exists in the Rust
     tree (the anchor string is grepped over crates/tdclient/src/td.rs and
     crates/tdclient/src/messages_manager.rs).

Exit 0 = the census is accounted for. Any missing row, unknown status or
unverifiable live anchor fails the audit.
"""
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TD_CPP = os.environ.get("TD_CPP", os.path.join(ROOT, "..", "td"))
CPP_FILE = os.path.join(TD_CPP, "td", "telegram", "MessagesManager.cpp")
STATUS_FILE = os.path.join(ROOT, "requests_gate", "m7_fanout_status.txt")
RUST_EVIDENCE_FILES = [
    os.path.join(ROOT, "crates", "tdclient", "src", "td.rs"),
    os.path.join(ROOT, "crates", "tdclient", "src", "messages_manager.rs"),
]


def cpp_intakes():
    with open(CPP_FILE, encoding="utf-8", errors="replace") as f:
        src = f.read()
    return sorted(
        set(re.findall(r"void MessagesManager::(on_update\w+)\(", src))
    )


def status_rows():
    rows = {}
    with open(STATUS_FILE, encoding="utf-8") as f:
        for line in f:
            line = line.rstrip("\n")
            if not line:
                continue
            parts = line.split("\t")
            if len(parts) < 3:
                raise SystemExit(f"BAD ROW (need name\\tstatus\\treason): {line!r}")
            name, status, reason = parts[0], parts[1], parts[2]
            if status not in ("live", "seam", "drop"):
                raise SystemExit(f"BAD STATUS {status!r} for {name}")
            if name in rows:
                raise SystemExit(f"DUPLICATE row for {name}")
            rows[name] = (status, reason)
    return rows


def rust_evidence():
    evidence = ""
    for path in RUST_EVIDENCE_FILES:
        with open(path, encoding="utf-8", errors="replace") as f:
            evidence += f.read()
    return evidence


def main():
    intakes = cpp_intakes()
    rows = status_rows()
    evidence = rust_evidence()

    failures = []
    missing = [name for name in intakes if name not in rows]
    if missing:
        failures.append(f"intakes with no status row: {missing}")
    stale = [name for name in rows if name not in intakes]
    if stale:
        failures.append(f"status rows for non-existent intakes (stale C++ pin?): {stale}")

    for name, (status, reason) in sorted(rows.items()):
        if status != "live":
            continue
        # the reason carries the evidence anchor: the last `word` token in
        # backticks is the Rust sink symbol to verify
        anchors = re.findall(r"`([A-Za-z_][A-Za-z0-9_]*)`", reason)
        if not anchors:
            failures.append(f"live row {name}: no Rust evidence anchor in reason")
            continue
        if not any(anchor in evidence for anchor in anchors):
            failures.append(
                f"live row {name}: none of the anchors {anchors} found in the Rust tree"
            )

    live = sum(1 for s, _ in rows.values() if s == "live")
    seam = sum(1 for s, _ in rows.values() if s == "seam")
    drop = sum(1 for s, _ in rows.values() if s == "drop")
    print(f"m7 fan-out census: {len(intakes)} C++ intakes = {live} live / {seam} seam / {drop} documented drop")
    if failures:
        print("AUDIT FAIL:")
        for failure in failures:
            print(" -", failure)
        return 1
    print("M7_FANOUT_AUDIT_GREEN")
    return 0


if __name__ == "__main__":
    sys.exit(main())
