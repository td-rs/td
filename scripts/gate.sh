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
# The lock fd stays off the gate's children (cargo runs with 9>&-): a
# daemonized child — the machine-shared sccache server, (re)started by
# cargo's sccache client — must not inherit it, or it outlives the run
# holding gate.lock and every later run queues behind the zombie (#157).
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

# Verification runs without sccache: the shared warm CARGO_TARGET_DIR carries
# the reuse (measured sccache Rust hit rate in gate runs: 0%), and a gate-
# spawned daemon must not exist in the first place — it is the thing that
# leaks the lock or eats a signal-15 mid-compile. Matches the env -u
# RUSTC_WRAPPER convention the conformance/fuzz verify scripts already
# follow (#157). The 9>&- fd discipline below stays as defense against any
# other daemonizing child.
unset RUSTC_WRAPPER

# Preflight the requested gates' toolchain needs before taking the lock
# (#157): an ambient cargo without the cargo-fmt/cargo-clippy subcommands
# (the nix-profile one) used to fail run_gate as command-not-found, and the
# run reported GATES_RED with the real stderr lost in the caller's `| tail`.
# Fail here, legibly, instead of queueing for a red run.
command -v cargo >/dev/null || die "cargo not on PATH"
for g in "${GATES[@]}"; do
  case $g in
    fmt|clippy)
      cargo "$g" --version >/dev/null 2>&1 ||
        die "ambient cargo ($(command -v cargo)) has no '$g' subcommand — the $g gate needs cargo-$g; put the pinned toolchain's bin (rust-toolchain.toml) first on PATH" ;;
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
# fd 9 holds the flock. Rule: every child spawned after this line closes it
# (9>&-) — a daemonized child that inherits the open file description
# outlives the run holding gate.lock (#157). The flock utilities are the
# exception: they operate on fd 9 itself.
exec 9>"$CACHE_DIR/gate.lock"

gate_lock_holders() {
  # Pids holding any fd on the lock file. `|| true`: find reports permission
  # errors on other users' /proc entries, which is normal, not a failure.
  # 9>&-: the scan's own children inherit fd 9 and must not scan as holders.
  find /proc/[0-9]*/fd -lname "$CACHE_DIR/gate.lock" 2>/dev/null 9>&- |
    cut -d/ -f3 9>&- | sort -un 9>&- || true
}

pid_owns_lock() { # $1 = pid; 0 when one of its fds on the lock file holds
                  # the acquired flock (the whole fork-inherited description
                  # shows the same fdinfo lock line)
  local f
  for f in /proc/"$1"/fd/*; do
    [[ $(readlink "$f" 2>/dev/null 9>&-) == "$CACHE_DIR/gate.lock" ]] || continue
    grep -q '^lock:' "/proc/$1/fdinfo/${f##*/}" 2>/dev/null 9>&- && return 0
  done
  return 1
}

gate_lock_leaks() { # fills LEAKS with lock owners that are safe to rescue
  # Ownership decides, and only owners are kill targets. A gate.sh process
  # that OWNS the flock means a live gate run legitimately holds it — LEAKS
  # is emptied and we queue normally (its cargo/rustc/sccache children share
  # the owning description and would be killed only together with it, never
  # alone). A non-gate.sh holder WITHOUT ownership is never a target: it is
  # foreign queued work (an old gate.sh blocks inside its own `flock 9`
  # utility) or a transient child, and killing it cannot release the lock
  # anyway. When no gate.sh owns the lock, the owning holders are leaks:
  # daemons outliving a pre-#157 run and their fork-sharers, which never
  # exit (the shared sccache server pins its idle timeout to 0).
  LEAKS=()
  local pid owns_gate=0
  # 9>&- on the substitution's function call: the scan subshell inherits
  # fd 9 and would otherwise match its own walk (its exit racing the
  # classification below). Always rc 0: the caller invokes it bare under
  # set -e.
  for pid in $(gate_lock_holders 9>&-); do
    # A holder that died between the scan and here is gone; killing it is a
    # no-op and classifying it would need its cmdline.
    [[ -r /proc/$pid/cmdline ]] || continue
    if [[ $(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null 9>&-) == *gate.sh* ]]; then
      if pid_owns_lock "$pid"; then owns_gate=1; fi
      continue
    fi
    pid_owns_lock "$pid" && LEAKS+=("$pid")
  done
  [[ $owns_gate == 0 ]] || LEAKS=()
}

DRAIN_TIMEOUT_S=900  # bound on waiting out a leaked server's attached compiles

drain_compile_children() { # $1 = leaked sccache server pid; 0 when quiet
  # Its in-flight compile children belong to other sessions; terminating the
  # server mid-compile SIGTERMs them and reddens those runs on pure infra
  # (the signal-15 rule). Wait for a quiet moment, bounded, before killing.
  # The quiet->kill window is not atomic: a compile attaching inside it is
  # lost — the same accepted race as an audited forced daemon stop.
  local kids i
  for i in $(seq 1 "$DRAIN_TIMEOUT_S"); do
    kids=$(pgrep -P "$1" 2>/dev/null 9>&-) || true
    [[ -z $kids ]] && return 0
    if [[ $i == 1 ]]; then
      echo "gate: waiting up to $DRAIN_TIMEOUT_S s for compiles attached to the leaked server (pid $1):"
      ps -o pid=,etime=,args= -p "$(paste -sd, <<<"$kids")" 2>/dev/null 9>&- | cut -c1-140 9>&- || true
    fi
    sleep 1 9>&-
  done
  echo "gate: compiles still attached to the leaked server (pid $1) after $DRAIN_TIMEOUT_S s" >&2
  return 1
}

rescue_lock() { # $@ = leaked holder pids; dies when the rescue cannot clear
  echo "gate: gate.lock held by non-gate process(es) $* — a daemon leaked from a pre-#157 run; rescuing"
  local pid i
  for pid in "$@"; do
    if [[ $(ps -p "$pid" -o comm= 2>/dev/null 9>&-) == sccache ]]; then
      drain_compile_children "$pid" ||
        die "gate.lock still held by the leaked sccache server (pid $pid) with compiles attached; see above"
    fi
    kill -TERM "$pid" 2>/dev/null || true
    # Confirm death before the next scan tick: a sccache shutdown can outlast
    # one flock -w tick, and an unchanged leak set must not be read as a
    # failed rescue. Bounded; a holder alive after the grace re-scans
    # unchanged and dies legibly in the acquire loop below.
    for i in {1..30}; do
      kill -0 "$pid" 2>/dev/null || break
      sleep 1 9>&-
    done
  done
}

# Acquire the machine lock, rescuing leaked holders while waiting (#157):
# a daemon can leak out of a live pre-#157 run at any moment, so a
# probe-once before queueing is not enough.
last_leaks=
noted=
until flock -w 10 9 2>/dev/null; do
  gate_lock_leaks
  if [[ ${#LEAKS[@]} -gt 0 ]]; then
    [[ ${LEAKS[*]} != "$last_leaks" ]] ||
      die "gate.lock still held by leaked process(es) ${LEAKS[*]} after the rescue attempt above; remove them manually"
    last_leaks=${LEAKS[*]}
    rescue_lock "${LEAKS[@]}"
  elif [[ -z $noted ]]; then
    noted=1
    echo "gate: gate.lock is held by a live gate run (or an invisible holder); queueing"
  fi
done

if [[ -d $GATE_DIR ]]; then
  [[ -f $GATE_DIR/.git ]] || die "$GATE_DIR exists but is not a git worktree; remove it first"
  # -f: a failed gate run leaves the worktree behind for inspection; this
  # checkout must self-heal from any edits made there.
  git -C "$GATE_DIR" checkout -f --detach --quiet "$SHA" 9>&-
  git -C "$GATE_DIR" clean -xdqf 9>&-
else
  git worktree add --detach "$GATE_DIR" "$SHA" 9>&-
fi

echo "gate: verifying $SHA in $GATE_DIR (target: $TARGET_DIR)"
cd "$GATE_DIR"
export CARGO_TARGET_DIR=$TARGET_DIR

run_gate() {
  # 9>&- (#157): keep the lock fd off cargo and everything it spawns — the
  # machine-shared sccache server is daemonized from cargo's sccache client,
  # and a daemon that inherits fd 9 outlives this script holding gate.lock.
  # The fd stays open in gate.sh itself, so the flock still spans the run.
  case $1 in
    fmt)    cargo fmt --all --check 9>&- ;;
    clippy) cargo clippy --workspace --all-targets --locked -- -D warnings 9>&- ;;
    doc)    RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --locked 9>&- ;;
    test)   cargo test --workspace --locked 9>&- ;;
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
