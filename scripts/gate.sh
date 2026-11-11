#!/usr/bin/env bash
# gate.sh — run the verification gates in a detached worktree (#148).
#
# The shared master checkout's dirty state is foreign in-flight work; clearing
# it (stash / checkout -f / clean) to run gates caused the 2026-11-11 stash
# collision incident. This script instead verifies a committed REV in a
# dedicated worktree and never touches the caller's working tree.
#
# The gate worktree lives at a fixed sibling path of the main checkout and
# shares one target dir across runs, so repeated gate runs stay warm; a
# per-machine flock serializes concurrent runs (they share both).
#
# Usage:
#   scripts/gate.sh [REV] [--only fmt,clippy,doc,test]
#     REV     commit-ish to verify (default: HEAD of the current checkout —
#             only the committed state is verified, never dirty files)
#     --only  comma-separated subset of gates (default: all four, in order)
#
# Environment overrides:
#   TD_GATE_DIR         gate worktree path (default: <main-checkout-sibling>/gate-td-rs-verify)
#   TD_GATE_TARGET_DIR  shared CARGO_TARGET_DIR (default: ~/.cache/td-rs/gate-target)
#
# Reclaim disk with: git worktree remove <TD_GATE_DIR> --force
set -euo pipefail

usage() { sed -n '2,/^set -euo/p' "$0" | head -n -1; }

die() { echo "gate.sh: $*" >&2; exit 2; }

ROOT=$(git rev-parse --show-toplevel 2>/dev/null) || die "not inside a git checkout"
cd "$ROOT"

ALL_GATES=(fmt clippy doc test)

REV=HEAD
ONLY=all
while [[ $# -gt 0 ]]; do
  case $1 in
    --only)
      [[ $# -ge 2 ]] || die "--only needs a value: one or more of ${ALL_GATES[*]}"
      ONLY=$2; shift 2 ;;
    --only=*) ONLY=${1#--only=}; shift ;;
    -h|--help) usage; exit 0 ;;
    *) REV=$1; shift ;;
  esac
done

SHA=$(git rev-parse --verify "${REV}^{commit}") || die "cannot resolve REV: $REV"

if [[ $ONLY == all ]]; then
  IFS=, ONLY="${ALL_GATES[*]}"
fi
IFS=, read -ra GATES <<<"$ONLY"
[[ ${#GATES[@]} -gt 0 ]] || die "--only needs at least one of ${ALL_GATES[*]}"
for g in "${GATES[@]}"; do
  case $g in
    fmt|clippy|doc|test) ;;
    *) die "unknown gate: $g" ;;
  esac
done

MAIN_ROOT=$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")
GATE_DIR=${TD_GATE_DIR:-$(dirname "$MAIN_ROOT")/gate-td-rs-verify}
TARGET_DIR=${TD_GATE_TARGET_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/td-rs/gate-target}
CACHE_DIR=${XDG_CACHE_HOME:-$HOME/.cache}/td-rs

command -v flock >/dev/null || die "flock (util-linux) is required"
mkdir -p "$CACHE_DIR" "$TARGET_DIR"

[[ $ROOT != "$GATE_DIR" && $GATE_DIR != "$ROOT"/* && $GATE_DIR != "$MAIN_ROOT"/* ]] \
  || die "gate worktree path must be outside every checkout tree: $GATE_DIR"

# Serialize gate runs per machine: they share the worktree and target dir.
exec 9>"$CACHE_DIR/gate.lock"
flock 9

if [[ -d $GATE_DIR ]]; then
  [[ -f $GATE_DIR/.git ]] || die "$GATE_DIR exists but is not a git worktree; remove it first"
  # -f: a failed gate run leaves the worktree behind for inspection; this
  # checkout must self-heal from any edits made there.
  git -C "$GATE_DIR" checkout -f --detach --quiet "$SHA"
  git -C "$GATE_DIR" clean -xdqf
else
  git worktree add --detach "$GATE_DIR" "$SHA"
fi

echo "gate: verifying $SHA in $GATE_DIR (target: $TARGET_DIR)"
cd "$GATE_DIR"
export CARGO_TARGET_DIR=$TARGET_DIR

run_gate() {
  case $1 in
    fmt)    cargo fmt --all --check ;;
    clippy) cargo clippy --workspace --all-targets --locked -- -D warnings ;;
    doc)    RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --locked ;;
    test)   cargo test --workspace --locked ;;
  esac
}

failed=()
for g in "${GATES[@]}"; do
  echo "==> gate $g"
  if run_gate "$g"; then echo "gate $g: OK"; else echo "gate $g: FAILED"; failed+=("$g"); fi
done

if [[ ${#failed[@]} -gt 0 ]]; then
  echo "GATES_RED: ${failed[*]} (rev $SHA; worktree kept at $GATE_DIR for inspection)"
  exit 1
fi
echo "GATES_GREEN: ${GATES[*]} at $SHA"
