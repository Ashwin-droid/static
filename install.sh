#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STATE_FILE="${TMPDIR:-/tmp}/static-force-fix-state-$(date +%Y%m%d-%H%M%S).json"
LOCAL_DESKTOP="${XDG_DATA_HOME:-$HOME/.local/share}/applications/com.static.music.desktop"
SYSTEM_BIN="/usr/bin/static"
CLEAN_BUILD=0

log() {
  printf '[force-fix] %s\n' "$*"
}

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    log "missing required command: $1"
    exit 1
  fi
}

usage() {
  cat <<'EOF'
Usage: install.sh [--clean] [--no-clean] [--help]

Options:
  --clean     Run `cargo clean` before rebuilding the package.
  --no-clean  Rebuild/install from the current target dir (default).
  --help      Show this help text.
EOF
}

python_dbus() {
  local mode="$1"
  local expect_up="${2:-}"
  MODE="$mode" STATE_FILE="$STATE_FILE" EXPECT_UP="$expect_up" python3 <<'PY'
import ast
import json
import os
import subprocess
import sys
import time

DEST = "com.static.music.staticd"
OBJ = "/com/static/music/Staticd"
METH = "com.static.music.Staticd1.RequestJson"

mode = os.environ["MODE"]
state_file = os.environ["STATE_FILE"]
expect_up_raw = os.environ.get("EXPECT_UP", "")


def request(payload):
    json_payload = json.dumps(payload)
    proc = subprocess.run(
        [
            "gdbus",
            "call",
            "--session",
            "--dest",
            DEST,
            "--object-path",
            OBJ,
            "--method",
            METH,
            json.dumps(json_payload),
        ],
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        raise RuntimeError(proc.stderr.strip() or proc.stdout.strip() or f"gdbus failed: {proc.returncode}")

    raw = proc.stdout.strip()
    value = ast.literal_eval(raw)
    if not isinstance(value, tuple) or len(value) != 1:
        raise RuntimeError(f"unexpected gdbus response shape: {raw}")
    return json.loads(value[0])


def wait_for_ping(expect_up: bool, timeout: float = 8.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            response = request("Ping")
            if expect_up and response == "Pong":
                return True
            if not expect_up:
                time.sleep(0.05)
        except Exception:
            if not expect_up:
                return True
        time.sleep(0.05)
    return False


if mode == "snapshot":
    state_response = request("GetState")
    state = state_response["State"]
    with open(state_file, "w") as handle:
        json.dump(state, handle)
    current_track = (state.get("current_track") or {}).get("video_id")
    print(f"saved {state_file}")
    print(f"current_track={current_track}")
    print(f"queue_index={state.get('queue_index')}")
    print(f"position_millis={state.get('position_millis')}")
    print(f"paused={state.get('paused')}")
elif mode == "shutdown":
    response = request("Shutdown")
    print(response)
elif mode == "wait":
    expect_up = expect_up_raw == "1"
    if not wait_for_ping(expect_up):
        raise SystemExit(1)
elif mode == "restore":
    with open(state_file) as handle:
        state = json.load(handle)

    request({"SetVolume": {"volume": state.get("volume", 1.0)}})
    queue = state.get("queue") or []
    if queue:
        request(
            {
                "PlayQueueAt": {
                    "queue": queue,
                    "index": min(state.get("queue_index", 0), max(len(queue) - 1, 0)),
                    "playlist_context": state.get("playlist_context"),
                    "radio_virtual_playlist": state.get("radio_virtual_playlist"),
                    "repeat_mode": state.get("repeat_mode", "Off"),
                }
            }
        )
        position_millis = state.get("position_millis")
        if isinstance(position_millis, int) and position_millis > 0:
            request({"SeekToMillis": {"position_millis": position_millis}})
        if state.get("paused"):
            request("Pause")

    restored = request("GetState")["State"]
    current_track = (restored.get("current_track") or {}).get("video_id")
    print(f"restored_track={current_track}")
    print(f"restored_queue_index={restored.get('queue_index')}")
    print(f"restored_position_millis={restored.get('position_millis')}")
    print(f"restored_paused={restored.get('paused')}")
else:
    raise SystemExit(f"unknown mode: {mode}")
PY
}

for cmd in cargo gdbus pkexec python3 pgrep; do
  require_cmd "$cmd"
done

while (($#)); do
  case "$1" in
    --clean)
      CLEAN_BUILD=1
      ;;
    --no-clean)
      CLEAN_BUILD=0
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      log "unknown argument: $1"
      usage
      exit 1
      ;;
  esac
  shift
done

log "root dir: $ROOT_DIR"
log "state file: $STATE_FILE"

daemon_was_running=0
if python_dbus snapshot; then
  daemon_was_running=1
  log "captured live daemon state"
else
  log "no live daemon state captured; continuing without restore"
fi

if [[ -e "$LOCAL_DESKTOP" ]]; then
  backup_path="${LOCAL_DESKTOP}.bak.$(date +%Y%m%d-%H%M%S)"
  mkdir -p "$(dirname "$LOCAL_DESKTOP")"
  cp "$LOCAL_DESKTOP" "$backup_path"
  rm -f "$LOCAL_DESKTOP"
  log "removed local desktop override; backup at $backup_path"
else
  log "no local desktop override found"
fi

if (( CLEAN_BUILD )); then
  log "building fresh package with cargo clean && cargo deb"
  (cd "$ROOT_DIR" && cargo clean && cargo deb)
else
  log "building fresh package with cargo deb --no-clean mode (default)"
  (cd "$ROOT_DIR" && cargo deb)
fi

DEB_PATH="$(
  find "$ROOT_DIR/target/debian" -maxdepth 1 -type f -name 'static_*.deb' -printf '%T@ %p\n' \
    | sort -n \
    | tail -n 1 \
    | cut -d' ' -f2-
)"

if [[ ! -f "$DEB_PATH" ]]; then
  log "expected deb not found at $DEB_PATH"
  exit 1
fi

log "installing $DEB_PATH via pkexec"
pkexec dpkg -i "$DEB_PATH"

if (( daemon_was_running )); then
  log "shutting down existing daemon"
  python_dbus shutdown >/dev/null

  if ! python_dbus wait 0; then
    log "daemon did not stop in time"
    exit 1
  fi

  log "starting packaged daemon from $SYSTEM_BIN"
  nohup "$SYSTEM_BIN" --daemon >/dev/null 2>&1 &

  if ! python_dbus wait 1; then
    log "new daemon did not start in time"
    exit 1
  fi

  log "restoring captured daemon state"
  python_dbus restore
fi

current_daemon="$(pgrep -af 'static --daemon' || true)"
if [[ -n "$current_daemon" ]]; then
  log "active daemon:"
  printf '%s\n' "$current_daemon"
fi

log "done"
