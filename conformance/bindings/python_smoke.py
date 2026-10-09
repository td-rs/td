#!/usr/bin/env python3
"""S81 bindings validation — runs the tdlib python example UNMODIFIED against
the Rust tdjson cdylib.

The example resolves its library through ctypes.util.find_library("tdjson");
this driver monkeypatches exactly that resolution to the Rust .so and drives
the example's own TdExample binding code (its ctypes signatures, its log
callback setup, its send/receive/execute wrappers) through a bounded smoke
over exactly the surface the shipped example uses (the ClientManager API):

  1. td_execute(getOption version)       -> optionValueString "1.8.68"
  2. td_create_client_id + td_receive    -> updateAuthorizationState
                                            (waitTdlibParameters at minimum)
  3. td_send(close)                      -> authorizationStateClosed,
                                            then a clean process exit

The example sources are NOT part of this repo: point argv[2] at an
`example/python` checkout of the pinned tdlib commit (the driver never
modifies them).

Usage: python3 python_smoke.py /path/to/libtdjson.so [/path/to/example/python]
Exit 0 = binding validated; anything else prints the failing step.
"""

import ctypes.util
import json
import os
import sys

SO_PATH = sys.argv[1] if len(sys.argv) > 1 else "libtdjson.so"
EXAMPLE_DIR = (
    sys.argv[2]
    if len(sys.argv) > 2
    else "/home/sadiq/tdrs-gates/td-bindings-example/python"
)


def fake_find_library(name):
    if name == "tdjson":
        return SO_PATH
    return None


def receive_until(example, predicate, timeout_s, what):
    """Bounded receive loop: give up (exit 2) rather than hang the audit."""
    seen = 0
    for _ in range(int(timeout_s * 4)):
        event = example.receive(0.25)
        if event is None:
            continue
        seen += 1
        if predicate(event):
            return event, seen
    print(f"FAIL: {what} not observed in {timeout_s}s ({seen} events seen)")
    sys.exit(2)


def main():
    # Route the example's library resolution to the Rust cdylib BEFORE the
    # module loads it. Everything else about the example runs as shipped.
    ctypes.util.find_library = fake_find_library
    sys.path.insert(0, os.path.abspath(EXAMPLE_DIR))
    from tdjson_example import TdExample  # noqa: E402

    print(f"== loading {SO_PATH} through the example's ctypes binding ==")
    example = TdExample()  # creates the client, installs the log callback

    # 1. synchronous execute through the example wrapper.
    version = example.execute({"@type": "getOption", "name": "version"})
    print(f"td_execute getOption version -> {json.dumps(version)}")
    if not (
        isinstance(version, dict)
        and version.get("@type") == "optionValueString"
        and version.get("value") == "1.8.68"
    ):
        print("FAIL: version parity — expected optionValueString 1.8.68")
        sys.exit(1)

    # 2. the async ClientManager path reaches its first authorization state.
    _, _ = receive_until(
        example,
        lambda e: e.get("@type") == "updateAuthorizationState"
        and e["authorization_state"].get("@type")
        == "authorizationStateWaitTdlibParameters",
        30,
        "authorizationStateWaitTdlibParameters",
    )
    print("updateAuthorizationState(waitTdlibParameters) received")

    # 3. the close machine settles in authorizationStateClosed. NOTE: close
    # currently waits out a pending pre-auth config fetch's natural network
    # timeout (~100 s) instead of cancelling it — the net-close-drain
    # divergence documented in bindings/README.md; the C++ settles in <1 s.
    example.send({"@type": "close"})
    _, _ = receive_until(
        example,
        lambda e: e.get("@type") == "updateAuthorizationState"
        and e["authorization_state"].get("@type") == "authorizationStateClosed",
        180,
        "authorizationStateClosed",
    )
    print("authorizationStateClosed received")

    # The example's surface ends here; the process must exit cleanly with
    # the closed manager client and the process-wide singleton still alive
    # (the C++ leaks the manager singleton the same way).
    print("PASS: tdlib python example runs unmodified against the Rust tdjson")


if __name__ == "__main__":
    main()
