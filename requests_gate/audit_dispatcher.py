#!/usr/bin/env python3
"""M.10.22 exhaustiveness audit, switch 1 — the td_api request dispatcher.

Cross-checks the census fixture
(`crates/tdclient/src/testdata/request_census.txt`, one `name<TAB>state`
line per scheme function) against the live `run_request` family chain
(`crates/tdclient/src/requests.rs`):

  1. every `dispatched` census row has a live `Function::<name>` arm;
  2. every `interim` census row has no arm — or only an arm whose body is
     the documented `NOT_IMPLEMENTED_YET` 500 placeholder (the
     `sendCallLog`-style parked stubs);
  3. every `Function::<name>` arm in requests.rs is a census-known function;
  4. the census pins in `mod request_census_tests` match the fixture
     counts (the pinned ledger the flip protocol bumps).

Exit 0 = the dispatcher switch is accounted for.
"""
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CENSUS = os.path.join(
    ROOT, "crates", "tdclient", "src", "testdata", "request_census.txt"
)
REQUESTS = os.path.join(ROOT, "crates", "tdclient", "src", "requests.rs")


def main():
    census = {}
    with open(CENSUS, encoding="utf-8") as f:
        for line in f:
            line = line.rstrip("\n")
            if not line:
                continue
            name, state = line.split("\t")
            census[name] = state

    with open(REQUESTS, encoding="utf-8") as f:
        src = f.read()

    failures = []

    # arm bodies for the interim-with-stub check. The lookbehind skips
    # path-qualified mtproto constructor calls (tdmtproto::telegram_api::
    # Function::messages_editMessage) — an arm is only an unqualified
    # `Function::<name>` match/variant in the run_request chain.
    arm_spans = {}
    for m in re.finditer(r"(?<!:)Function::(\w+)\s*(?:\(|=>)", src):
        name = m.group(1)
        arm_spans.setdefault(name, m.start())

    for name, state in census.items():
        if state == "dispatched":
            if name not in arm_spans:
                failures.append(f"census dispatched but no arm: {name}")
        elif state == "interim":
            if name in arm_spans:
                body = src[arm_spans[name] : arm_spans[name] + 1200]
                if "NOT_IMPLEMENTED_YET" not in body:
                    failures.append(f"census interim but has a live arm: {name}")

    known_census_arms = {
        name
        for name, state in census.items()
        if state in ("dispatched", "interim")
    }
    for name in sorted(arm_spans):
        if name not in census:
            failures.append(f"arm with no census row: {name}")
        known_census_arms.discard(name)

    # pinned counts
    counts = {}
    for state in census.values():
        counts[state] = counts.get(state, 0) + 1
    for state, pin in re.findall(
        r'counts\.get\("(\w+)"\)\.unwrap_or\(&0\),?\s*(\d+)', src
    ):
        actual = counts.get(state, 0)
        if int(pin) != actual:
            failures.append(
                f"census pin {state}={pin} does not match the fixture ({actual})"
            )

    print(
        f"dispatcher census: {len(census)} scheme functions = "
        f"{counts.get('sync', 0)} sync / {counts.get('dispatched', 0)} dispatched / "
        f"{counts.get('interim', 0)} interim"
    )
    if failures:
        print("AUDIT FAIL:")
        for failure in failures:
            print(" -", failure)
        return 1
    print("DISPATCHER_AUDIT_GREEN")
    return 0


if __name__ == "__main__":
    sys.exit(main())
