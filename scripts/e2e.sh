#!/bin/sh
# Sandboxed end-to-end check: hook -> daemon -> LEDS.LED on a fake device.
# Never touches real devices, power settings, or the running SidePulse install.
set -eu

BIN=${1:-target/release/sidepulse}
SETTINGS_SOURCE=${2:-}
SANDBOX=$(mktemp -d /tmp/sp-e2e.XXXXXX)
DEVICE="$SANDBOX/volumes/SidePulse"
mkdir -p "$DEVICE" "$SANDBOX/state" "$SANDBOX/config/animations"
printf 'serial SPP-000001\nrelease_version 1.1.0\n' > "$DEVICE/STATUS.TXT"
printf 'off' > "$DEVICE/LEDS.LED"

if [ -n "$SETTINGS_SOURCE" ]; then
  cp "$SETTINGS_SOURCE" "$SANDBOX/config/settings.json"
  cp "$(dirname "$SETTINGS_SOURCE")"/animations/*.LED "$SANDBOX/config/animations/" 2>/dev/null || true
else
  printf '{"agent_animations":{"completed":{"style":"solid-green"}}}\n' > "$SANDBOX/config/settings.json"
fi

export SIDEPULSE_RS_STATE_DIR="$SANDBOX/state"
export SIDEPULSE_SETTINGS="$SANDBOX/config/settings.json"
export SIDEPULSE_MOUNT_ROOTS="$SANDBOX/volumes"
export SIDEPULSE_NO_POWER_CONTROL=1

"$BIN" daemon --no-menubar 2> "$SANDBOX/daemon.log" &
DAEMON=$!
trap 'kill $DAEMON 2>/dev/null || true; rm -rf "$SANDBOX"' EXIT

for _ in 1 2 3 4 5 6 7 8 9 10; do
  [ -S "$SANDBOX/state/daemon.sock" ] && break
  sleep 0.1
done

FAILED=0
step() {
  label=$1 payload=$2 expect_mode=$3
  printf '%s' "$payload" | "$BIN" hook --provider claude
  sleep 0.3
  mode=$("$BIN" status --json | sed -n 's/.*"mode": *"\([a-z_]*\)".*/\1/p' | head -1)
  first=$(head -1 "$DEVICE/LEDS.LED")
  if [ "$mode" = "$expect_mode" ]; then result=ok; else result="FAIL (got $mode)"; FAILED=1; fi
  printf '%-18s %-18s %-6s LEDS.LED: %s\n' "$label" "$expect_mode" "$result" "$first"
}

S='"session_id":"e2e-session","cwd":"/tmp"'
step SessionStart      "{\"hook_event_name\":\"SessionStart\",$S}" idle_ready
step UserPromptSubmit  "{\"hook_event_name\":\"UserPromptSubmit\",$S,\"prompt\":\"Port it to Rust\"}" working
step PreToolUse        "{\"hook_event_name\":\"PreToolUse\",$S,\"tool_name\":\"Bash\"}" tool_running
step PermissionRequest "{\"hook_event_name\":\"PermissionRequest\",$S,\"tool_name\":\"Bash\"}" waiting_for_input
step PostToolUse       "{\"hook_event_name\":\"PostToolUse\",$S,\"tool_response\":{\"exit_code\":0}}" working
step Stop              "{\"hook_event_name\":\"Stop\",$S,\"last_assistant_message\":\"Done.\"}" completed
step Notification      "{\"hook_event_name\":\"Notification\",$S,\"message\":\"Claude is waiting for your input\"}" completed

"$BIN" status
kill -TERM $DAEMON
wait $DAEMON 2>/dev/null || true
[ -f "$SANDBOX/state/sessions.json" ] && echo "sessions persisted: $(wc -c < "$SANDBOX/state/sessions.json") bytes"
echo "--- daemon log"
cat "$SANDBOX/daemon.log"
exit $FAILED
