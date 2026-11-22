#!/usr/bin/env bash
# Independent HTTP adapter lifecycle. Never use daemon's child supervisor.
# Usage: web-service.sh <runtime-directory> <start|stop|restart|status|prepare>
set -euo pipefail
[ "$#" -eq 2 ] || { echo 'usage: web-service.sh <runtime-directory> <start|stop|restart|status|prepare>' >&2; exit 2; }
ROOT=$(realpath "$1")
ACTION=$2
BIN="$ROOT/bin/mozart-web-adapter"
CFG=$(realpath "$ROOT/mozart.toml")
RUN="$ROOT/run"
PIDFILE="$RUN/web-adapter.pid"
# Existing daemon directories can be 0755, but must not be writable by others.
validate_dir() {
    local mode owner
    if [ -L "$1" ] || [ ! -d "$1" ]; then echo "unsafe directory: $1" >&2; return 1; fi
    read -r owner mode < <(stat -c '%u %a' "$1")
    if [ "$owner" != "$(id -u)" ] || (( (8#$mode & 0022) != 0 )); then
        echo "directory must be owned by the current user and not group/world writable: $1" >&2; return 1
    fi
}
if [ "$ACTION" = status ]; then
    if [ ! -e "$RUN" ] && [ ! -L "$RUN" ]; then echo 'adapter stopped'; exit 1; fi
    validate_dir "$RUN"
else
    if [ ! -e "$RUN" ] && [ ! -L "$RUN" ]; then (umask 077; mkdir "$RUN"); fi
    validate_dir "$RUN"
    if [ ! -e "$RUN/logs" ] && [ ! -L "$RUN/logs" ]; then (umask 077; mkdir "$RUN/logs"); fi
    validate_dir "$RUN/logs"
    [ ! -L "$RUN/web-service.lock" ] || { echo 'unsafe lock file' >&2; exit 1; }
    exec 9>"$RUN/web-service.lock"
    flock -x 9
fi

starttime() {
    local stat tail
    read -r stat < "/proc/$1/stat" || return 1
    tail=${stat##*) }
    local -a fields
    read -r -a fields <<< "$tail"
    printf '%s' "${fields[19]}"
}
identity() {
    local record executable now
    [ -f "$PIDFILE" ] || return 1
    read -r record < "$PIDFILE" || return 1
    [[ $record =~ ^([1-9][0-9]*):([0-9]+)$ ]] || return 1
    PID=${BASH_REMATCH[1]}
    local expected=${BASH_REMATCH[2]}
    now=$(starttime "$PID") || return 1
    [ "$now" = "$expected" ] || return 1
    executable=$(readlink "/proc/$PID/exe") || return 1
    [[ $executable = "$BIN" || $executable = "$BIN (deleted)" ]] || return 1
    local -a arguments
    mapfile -d '' -t arguments < "/proc/$PID/cmdline" || return 1
    local i
    for ((i=1; i+1<${#arguments[@]}; i++)); do
        if [ "${arguments[i]}" = --config ] && [ "${arguments[i+1]}" = "$CFG" ]; then return 0; fi
    done
    return 1
}
prepare() {
    [ "$(id -u)" -ne 0 ] || { echo 'run the adapter as the runtime owner, not root' >&2; return 1; }
    local description port
    description=$("$BIN" --config "$CFG" --print-config)
    port=$(jq -er '.port' <<< "$description")
    if [ "$port" -gt 0 ] && [ "$port" -lt 1024 ]; then
        if ! getcap "$BIN" | grep -q 'cap_net_bind_service=ep'; then
            command -v sudo >/dev/null || { echo 'low HTTP port requires cap_net_bind_service or a configured high port' >&2; return 1; }
            sudo -n setcap cap_net_bind_service=ep "$BIN" || { echo 'grant cap_net_bind_service, or configure a high web.listen port' >&2; return 1; }
        fi
    fi
}
stop() {
    if ! identity; then rm -f "$PIDFILE"; return 0; fi
    kill -TERM "$PID"
    local deadline=$((SECONDS + 180))
    while identity; do
        if [ "$SECONDS" -ge "$deadline" ]; then echo 'adapter shutdown timed out; process left running' >&2; return 1; fi
        sleep 0.1
    done
    rm -f "$PIDFILE"
}
start() {
    if identity; then echo "adapter running: $PID"; return 0; fi
    prepare
    local handshake pid deadline record
    handshake=$(mktemp -d "$RUN/.web-start.XXXXXXXX")
    chmod 700 "$handshake"
    # Close the management lock in the child. Startup is acknowledged only after bind.
    nohup "$BIN" --config "$CFG" --ready-file "$handshake/ready.json" \
        >>"$RUN/logs/web-adapter.log" 2>&1 9>&- </dev/null &
    pid=$!
    deadline=$((SECONDS + 30))
    while ! { [ -s "$handshake/ready.json" ] && jq -e --argjson pid "$pid" '.pid == $pid' "$handshake/ready.json" >/dev/null 2>&1; }; do
        if ! kill -0 "$pid" 2>/dev/null; then
            echo 'adapter startup failed:' >&2
            tail -20 "$RUN/logs/web-adapter.log" >&2
            rm -rf "$handshake"
            return 1
        fi
        if [ "$SECONDS" -ge "$deadline" ]; then
            # This PID is our unreaped direct child, so it cannot have been reused.
            kill -TERM "$pid" 2>/dev/null || true
            wait "$pid" || true
            rm -rf "$handshake"
            echo 'adapter did not acknowledge startup' >&2
            return 1
        fi
        sleep 0.1
    done
    record="$pid:$(starttime "$pid")"
    printf '%s\n' "$record" > "$handshake/pid"
    mv "$handshake/pid" "$PIDFILE"
    rm -rf "$handshake"
    identity || { echo 'adapter exited after startup acknowledgment' >&2; return 1; }
    echo "adapter started: $PID"
}
case "$ACTION" in
    prepare) prepare ;;
    start) start ;;
    stop) stop ;;
    restart) stop; start ;;
    status) if identity; then echo "adapter running: $PID"; else echo 'adapter stopped'; exit 1; fi ;;
    *) echo "unknown action: $ACTION" >&2; exit 2 ;;
esac
