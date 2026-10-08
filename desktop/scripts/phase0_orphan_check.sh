#!/usr/bin/env bash
# Phase 0 orphan guard + warm-start timing.
#
#   V11  warm start (frontend build already cached) -> ready time
#   V9   force-quitting the parent must take the launcher and its two children
#        down within a few seconds, with no prompts and no leftovers
#
# Usage: bash desktop/scripts/phase0_orphan_check.sh
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HOME_DIR="${PHASE0_HOME:-$REPO_ROOT/.phase0-home}"
LOG_DIR="${PHASE0_LOG_DIR:-$REPO_ROOT/.phase0-logs}"
PYTHON="${PHASE0_PYTHON:-$REPO_ROOT/.venv/bin/python}"
INFO="$HOME_DIR/desktop/runtime.json"
DEADLINE="${PHASE0_READY_TIMEOUT:-600}"
WRAPPER=""
LAUNCHER=""

cleanup() {
  [ -n "$LAUNCHER" ] && kill "$LAUNCHER" 2>/dev/null
  [ -n "$WRAPPER" ] && kill "$WRAPPER" 2>/dev/null
  return 0
}
trap cleanup EXIT

runtime_field() {
  "$PYTHON" -c 'import json,pathlib,sys
p = pathlib.Path(sys.argv[1])
print(json.loads(p.read_text()).get(sys.argv[2], "") if p.exists() else "")' "$INFO" "$1"
}

wait_ready() {
  local start="$1" status=""
  while true; do
    status="$(runtime_field status)"
    if [ "$status" = "ready" ] || [ "$status" = "stopped" ]; then break; fi
    if [ $(( $(date +%s) - start )) -gt "$DEADLINE" ]; then status="timeout"; break; fi
    sleep 1
  done
  printf '%s' "$status"
}

mkdir -p "$HOME_DIR" "$LOG_DIR"

echo "[1/4] warm start timing (V11)"
rm -f "$INFO"
START=$(date +%s)
DEEPTUTOR_DESKTOP_SHELL=1 "$PYTHON" -m deeptutor_cli.main start \
  --home "$HOME_DIR" --no-browser --auto-ports --runtime-info "$INFO" \
  >"$LOG_DIR/launcher-warm.log" 2>&1 &
LAUNCHER=$!
status="$(wait_ready "$START")"
WARM=$(( $(date +%s) - START ))
echo "      status=$status  warm ready in ${WARM}s  (ports: $(runtime_field backend_port)/$(runtime_field frontend_port))"
if [ "$status" != "ready" ]; then
  echo "FAIL: warm start never reported ready"
  tail -20 "$LOG_DIR/launcher-warm.log"
  exit 1
fi
kill -TERM "$LAUNCHER" 2>/dev/null
for _ in $(seq 1 60); do kill -0 "$LAUNCHER" 2>/dev/null || break; sleep 1; done
LAUNCHER=""
sleep 1

echo "[2/4] start the launcher under a disposable parent"
rm -f "$INFO"
PIDS="$LOG_DIR/pids.txt"
: >"$PIDS"
bash -c 'DEEPTUTOR_DESKTOP_SHELL=1 "$1" -m deeptutor_cli.main start \
    --home "$2" --no-browser --auto-ports --runtime-info "$3" --parent-pid $$ \
    >"$4" 2>&1 &
  echo "$!" > "$5"
  while true; do sleep 5; done' _ "$PYTHON" "$HOME_DIR" "$INFO" "$LOG_DIR/launcher-orphan.log" "$PIDS" &
WRAPPER=$!
START=$(date +%s)
status="$(wait_ready "$START")"
echo "      status=$status  wrapper=$WRAPPER  launcher=$(cat "$PIDS" 2>/dev/null)"
if [ "$status" != "ready" ]; then
  echo "FAIL: launcher never became ready under the disposable parent"
  tail -20 "$LOG_DIR/launcher-orphan.log"
  exit 1
fi

echo "[3/4] SIGKILL the parent (simulates Force Quit / Task Manager)"
kill -9 "$WRAPPER" 2>/dev/null
WRAPPER=""
KILL_AT=$(date +%s)
ORPHAN_PID="$(cat "$PIDS" 2>/dev/null)"
GONE_AFTER=""
for _ in $(seq 1 60); do
  if ! kill -0 "$ORPHAN_PID" 2>/dev/null; then GONE_AFTER=$(( $(date +%s) - KILL_AT )); break; fi
  sleep 1
done
leftovers="$(pgrep -f "deeptutor.api.main" || true)"
echo "      launcher gone after: ${GONE_AFTER:->60}s"
echo "      leftover backend pids: ${leftovers:-none}"
echo "      runtime.json status: $(runtime_field status)"

echo "[4/4] summary"
if [ -n "$GONE_AFTER" ] && [ -z "$leftovers" ]; then
  echo "      PASS: orphan guard works (parent death -> full shutdown in ${GONE_AFTER}s)"
else
  echo "      FAIL: orphan guard did not fully clean up"
  exit 1
fi
