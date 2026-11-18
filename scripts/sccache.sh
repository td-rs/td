#!/usr/bin/env bash
# sccache.sh — pgrep-audited control of the machine-shared sccache server (#151).
#
# One sccache server is shared by every concurrent session and worktree on a
# machine (RUSTC_WRAPPER=sccache, TCP 127.0.0.1:4226). A bare
# `sccache --stop-server` (or `--restart-server`) while other sessions compile
# SIGTERMs the server's in-flight compile children — cargo then reports
# `sccache: Compile terminated by signal 15` and the run goes red on pure
# infra. The stock server can also idle-exit with jobs still attached
# (SCCACHE_IDLE_TIMEOUT, 600 s default).
#
# This wrapper makes both safe:
#   - stop/restart audit the server's compile children (pgrep -P) and refuse
#     while any are attached; --force overrides with a census printed;
#   - start pins SCCACHE_IDLE_TIMEOUT=0 (0 disables the timer) so the shared
#     server never idle-exits mid-compile.
#
# Usage:
#   scripts/sccache.sh status
#                     server pid/uptime, effective idle-timeout, and the
#                     in-flight compile census
#   scripts/sccache.sh start
#                     start the shared server with SCCACHE_IDLE_TIMEOUT=0
#                     (no-op, with a note, if one is already running)
#   scripts/sccache.sh stop [--force]
#                     audited stop; refuses while compiles are attached
#   scripts/sccache.sh restart [--force]
#                     audited stop + hardened start
#   scripts/sccache.sh wait-idle [--timeout SECS]
#                     block until no compiles are attached (default 900 s)
#
# Environment:
#   SCCACHE_SERVER_PORT  server discovery port (default 4226)
#
# Discovery assumes the stock TCP server and only sees this user's listener
# (ss -p shows pids for own sockets); a Unix-socket server
# (SCCACHE_SERVER_UDS) is not visible to this audit. The audit→stop window
# is not atomic: a session may attach a compile after the audit passes — the
# audit runs immediately before the stop to keep it small, --force documents
# intent.
set -euo pipefail

usage() { sed -n '2,/^set -euo/p' "$0" | head -n -1; }

die() { echo "sccache.sh: $*" >&2; exit 2; }

PORT=${SCCACHE_SERVER_PORT:-4226}

command -v sccache >/dev/null || die "sccache is required"
command -v ss >/dev/null || die "ss (iproute2) is required"
command -v pgrep >/dev/null || die "pgrep (procps) is required"

# Pid of the process listening on the server port; empty if none.
server_pid() {
  local pids
  # `|| true`: no listener means grep finds nothing (rc 1); an empty pid is
  # this function's normal "not running" answer, not a failure.
  pids=$(ss -Hltnp "sport = :$PORT" 2>/dev/null | grep -oE 'pid=[0-9]+' | cut -d= -f2) || true
  [[ $(grep -c . <<<"$pids") -le 1 ]] || die "multiple listeners on port $PORT: $pids"
  local pid=${pids:-}
  if [[ -n $pid ]]; then
    [[ $(ps -p "$pid" -o comm= 2>/dev/null) == sccache ]] \
      || die "port $PORT is owned by a non-sccache process (pid $pid)"
  fi
  echo "$pid"
}

# Pids of the server's direct children — the in-flight compile jobs.
compile_pids() {
  pgrep -P "$1" || true
}

census() { # $1 = newline-separated compile pids
  # `|| true`: every listed pid may finish between pgrep and this ps.
  [[ -z $1 ]] || ps -o pid=,etime=,args= -p "$(paste -sd, <<<"$1")" | cut -c1-140 || true
}

audit() { # $1 = action being refused; dies under load unless FORCE=1
  local spid pids
  spid=$(server_pid)
  [[ -n $spid ]] || return 0
  pids=$(compile_pids "$spid")
  [[ -z $pids ]] && return 0
  echo "in-flight compiles attached to the shared server (pid $spid):"
  census "$pids"
  if [[ $FORCE == 1 ]]; then
    echo "sccache.sh: --force: $1 anyway — expect signal-15 failures above" >&2
  else
    die "refusing to $1 while compiles are attached; wait-idle, retry when idle, or pass --force"
  fi
}

do_start() {
  local spid
  spid=$(server_pid)
  if [[ -n $spid ]]; then
    echo "server already running (pid $spid); leaving it as-is"
    note_idle_timeout "$spid"
    return 0
  fi
  SCCACHE_IDLE_TIMEOUT=0 sccache --start-server
  local _
  for _ in {1..20}; do
    spid=$(server_pid)
    [[ -n $spid ]] && break
    sleep 0.25
  done
  [[ -n $spid ]] || die "server did not come up on port $PORT"
  echo "server up (pid $spid) with SCCACHE_IDLE_TIMEOUT=0 (no idle exit)"
}

do_stop() {
  local spid rc
  spid=$(server_pid)
  if [[ -z $spid ]]; then
    echo "server not running"
    return 0
  fi
  audit "stop the shared server"
  if sccache --stop-server; then
    return 0
  else
    rc=$?
  fi
  # Observed rc 2 with "couldn't connect to server": the server vanished
  # between audit and stop — already stopped, nothing to kill.
  [[ $rc == 2 ]] || die "sccache --stop-server failed (rc=$rc)"
}

note_idle_timeout() { # $1 = server pid
  local val
  val=$(tr '\0' '\n' <"/proc/$1/environ" 2>/dev/null | grep '^SCCACHE_IDLE_TIMEOUT=' || true)
  if [[ $val == SCCACHE_IDLE_TIMEOUT=0 ]]; then
    echo "idle-timeout: $val in server env (hardened: no idle exit)"
  else
    echo "idle-timeout: ${val:-unset} — server may idle-exit (default 600 s);" \
      "harden with: scripts/sccache.sh restart" >&2
  fi
}

do_status() {
  local spid pids n
  spid=$(server_pid)
  if [[ -z $spid ]]; then
    echo "server: not running (port $PORT)"
    return 0
  fi
  echo "server: pid $spid, up $(ps -p "$spid" -o etime= | tr -d ' ') ($(sccache --version))"
  note_idle_timeout "$spid"
  pids=$(compile_pids "$spid")
  n=$(grep -c . <<<"$pids" || true)
  echo "in-flight compiles: ${n:-0}"
  census "$pids"
}

do_wait_idle() {
  local spid deadline n
  deadline=$((SECONDS + TIMEOUT))
  while :; do
    spid=$(server_pid)
    if [[ -z $spid ]]; then
      echo "idle (server not running)"
      return 0
    fi
    n=$(grep -c . <<<"$(compile_pids "$spid")" || true)
    if [[ ${n:-0} -eq 0 ]]; then
      echo "idle (server pid $spid, no compiles attached)"
      return 0
    fi
    ((SECONDS < deadline)) || die "timed out after ${TIMEOUT}s with $n compile(s) attached"
    sleep 2
  done
}

FORCE=0
TIMEOUT=900
CMD=
while [[ $# -gt 0 ]]; do
  case $1 in
    --force) FORCE=1; shift ;;
    --timeout)
      [[ $# -ge 2 ]] || die "--timeout needs a value: seconds"
      TIMEOUT=$2; shift 2 ;;
    --timeout=*) TIMEOUT=${1#--timeout=}; shift ;;
    -h|--help) usage; exit 0 ;;
    *)
      [[ -z $CMD ]] || die "unknown or extra argument: $1"
      CMD=$1; shift ;;
  esac
done

[[ $TIMEOUT =~ ^[0-9]+$ ]] || die "--timeout wants seconds (got: $TIMEOUT)"

case $CMD in
  status)    do_status ;;
  start)     do_start ;;
  stop)      do_stop ;;
  restart)   do_stop; do_start ;;
  wait-idle) do_wait_idle ;;
  *)
    usage >&2
    if [[ -n $CMD ]]; then die "unknown command: $CMD"; fi
    die "missing command" ;;
esac
