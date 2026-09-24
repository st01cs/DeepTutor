#!/usr/bin/env bash
# Phase 0 handshake check — no Tauri involved.
#
# Verifies the additive launcher contract on the real stack:
#   V10  occupied default ports are moved automatically (never prompts)
#   V2   --runtime-info publishes starting -> ready -> stopped
#   V8   SIGTERM stops the launcher and both children (no leftovers)
#
# Usage: bash desktop/scripts/phase0_handshake_check.sh
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HOME_DIR="${PHASE0_HOME:-$REPO_ROOT/.phase0-home}"
LOG_DIR="${PHASE0_LOG_DIR:-$REPO_ROOT/.phase0-logs}"
PYTHON="${PHASE0_PYTHON:-$REPO_ROOT/.venv/bin/python}"
INFO="$HOME_DIR/desktop/runtime.json"
DEADLINE="${PHASE0_READY_TIMEOUT:-900}"
HOLD_A=""
HOLD_B=""
LAUNCHER=""

cleanup() {
  [ -n "$LAUNCHER" ] && kill "$LAUNCHER" 2>/dev/null
  [ -n "$HOLD_A" ] && kill "$HOLD_A" 2>/dev/null
  [ -n "$HOLD_B" ] && kill "$HOLD_B" 2>/dev/null
  return 0
}
trap cleanup EXIT

runtime_field() {
  "$PYTHON" -c 'import json,pathlib,sys
p = pathlib.Path(sys.argv[1])
print(json.loads(p.read_text()).get(sys.argv[2], "") if p.exists() else "")' "$INFO" "$1"
}

mkdir -p "$HOME_DIR" "$LOG_DIR"
rm -f "$INFO"

echo "[1/6] occupy the default ports 8001 / 3782"
nc -k -l 8001 >/dev/null 2>&1 &
HOLD_A=$!
nc -k -l 3782 >/dev/null 2>&1 &
HOLD_B=$!
sleep 1

echo "[2/6] start the launcher with the desktop flags"
DEEPTUTOR_DESKTOP_SHELL=1 "$PYTHON" -m deeptutor_cli.main start \
  --home "$HOME_DIR" --no-browser --auto-ports \
  --runtime-info "$INFO" --parent-pid $$ \
  >"$LOG_DIR/launcher.log" 2>&1 &
LAUNCHER=$!

START=$(date +%s)
status=""
while true; do
  status="$(runtime_field status)"
  if [ "$status" = "ready" ] || [ "$status" = "stopped" ]; then break; fi
  if ! kill -0 "$LAUNCHER" 2>/dev/null; then status="exited"; break; fi
  if [ $(( $(date +%s) - START )) -gt "$DEADLINE" ]; then status="timeout"; break; fi
  sleep 2
done
ELAPSED=$(( $(date +%s) - START ))
echo "      status=$status  (${ELAPSED}s)"
if [ "$status" != "ready" ]; then
  echo "FAIL: launcher never reported ready"
  tail -30 "$LOG_DIR/launcher.log"
  exit 1
fi

BPORT="$(runtime_field backend_port)"
FPORT="$(runtime_field frontend_port)"
FURL="$(runtime_field frontend_url)"
echo "      backend=$BPORT frontend=$FPORT url=$FURL"

echo "[3/6] occupied defaults must have moved"
if [ "$BPORT" = "8001" ] || [ "$FPORT" = "3782" ]; then
  echo "FAIL: auto-ports did not move the occupied port(s)"
  exit 1
fi
if ! grep -q "\"backend_port\": $BPORT" "$HOME_DIR/data/user/settings/system.json"; then
  echo "FAIL: new ports were not persisted to system.json"
  grep -n "port" "$HOME_DIR/data/user/settings/system.json" || true
  exit 1
fi
echo "      persisted to data/user/settings/system.json"

echo "[4/6] HTTP readiness"
health="$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$BPORT/health/ready" || true)"
frontend="$(curl -s -o /dev/null -w '%{http_code}' "$FURL/" || true)"
echo "      /health/ready=$health  frontend=$frontend"
if [ "$health" != "200" ] || [ "$frontend" != "200" ]; then
  echo "FAIL: HTTP readiness failed"
  exit 1
fi

echo "[5/6] SIGTERM shuts the whole stack down"
kill -TERM "$LAUNCHER" 2>/dev/null
STOP_DEADLINE=$(( $(date +%s) + 60 ))
while kill -0 "$LAUNCHER" 2>/dev/null && [ "$(date +%s)" -lt "$STOP_DEADLINE" ]; do sleep 1; done
sleep 2
leftovers="$(pgrep -f "deeptutor.api.main" | grep -v "^$$\$" || true)"
echo "      launcher alive: $(kill -0 "$LAUNCHER" 2>/dev/null && echo yes || echo no)"
echo "      leftover pids: ${leftovers:-none}"
stopped="$(runtime_field status)"
echo "      runtime.json status=$stopped"

echo "[6/6] summary"
echo "      ports        : backend=$BPORT frontend=$FPORT"
echo "      ready in     : ${ELAPSED}s"
echo "      log          : $LOG_DIR/launcher.log"
echo "      runtime info : $INFO"
