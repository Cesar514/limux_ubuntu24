#!/usr/bin/env bash
# summary: Profile a staged Limux host runtime under an isolated XDG environment.
# purpose: Measure startup/control latency, idle CPU, RSS stability, and idle disk writes.
# inputs: LIMUX_PROFILE_HOST_BIN and LIMUX_PROFILE_CLI_BIN, or release binaries in target/release.
# returns/effects: Launches Limux temporarily, prints key=value metrics, and removes temp runtime files.

set -euo pipefail

HOST_BIN="${LIMUX_PROFILE_HOST_BIN:-target/release/limux}"
CLI_BIN="${LIMUX_PROFILE_CLI_BIN:-target/release/limux-cli}"
SAMPLE_SECONDS="${LIMUX_PROFILE_SAMPLE_SECONDS:-15}"
SAMPLE_INTERVAL="${LIMUX_PROFILE_SAMPLE_INTERVAL:-1}"
WARMUP_SECONDS="${LIMUX_PROFILE_WARMUP_SECONDS:-5}"

if [[ ! -x "$HOST_BIN" ]]; then
  echo "FATAL: host binary is not executable: $HOST_BIN" >&2
  exit 1
fi

if [[ ! -x "$CLI_BIN" ]]; then
  echo "FATAL: CLI binary is not executable: $CLI_BIN" >&2
  exit 1
fi

if [[ -z "${DISPLAY:-}" ]]; then
  echo "FATAL: DISPLAY is required for GTK runtime profiling" >&2
  exit 1
fi

if ! command -v awk >/dev/null 2>&1; then
  echo "FATAL: awk is required for runtime profiling" >&2
  exit 1
fi

if ! command -v getconf >/dev/null 2>&1; then
  echo "FATAL: getconf is required for runtime profiling" >&2
  exit 1
fi

PROFILE_ROOT="$(mktemp -d)"
HOST_PID=""

cleanup() {
  if [[ -n "$HOST_PID" ]] && kill -0 "$HOST_PID" >/dev/null 2>&1; then
    kill "$HOST_PID" >/dev/null 2>&1
    set +e
    wait "$HOST_PID" >/dev/null 2>&1
    set -e
  fi
  rm -rf "$PROFILE_ROOT"
}

trap cleanup EXIT

mkdir -p "$PROFILE_ROOT/data" "$PROFILE_ROOT/state" "$PROFILE_ROOT/runtime"
chmod 700 "$PROFILE_ROOT/runtime"

export XDG_DATA_HOME="$PROFILE_ROOT/data"
export XDG_STATE_HOME="$PROFILE_ROOT/state"
export XDG_RUNTIME_DIR="$PROFILE_ROOT/runtime"
export LIMUX_SOCKET="$PROFILE_ROOT/limux.sock"
export LIMUX_SOCKET_PATH="$LIMUX_SOCKET"
export LIMUX_SOCKET_MODE="localUser"
export GDK_BACKEND="${GDK_BACKEND:-x11}"

"$HOST_BIN" >"$PROFILE_ROOT/host.stdout" 2>"$PROFILE_ROOT/host.stderr" &
HOST_PID="$!"

socket_deadline=$((SECONDS + 10))
while [[ ! -S "$LIMUX_SOCKET" ]]; do
  if ! kill -0 "$HOST_PID" >/dev/null 2>&1; then
    echo "FATAL: Limux host exited before creating control socket" >&2
    if [[ -s "$PROFILE_ROOT/host.stderr" ]]; then
      cat "$PROFILE_ROOT/host.stderr" >&2
    fi
    exit 1
  fi
  if (( SECONDS >= socket_deadline )); then
    echo "FATAL: Limux host did not create control socket within 10 seconds" >&2
    if [[ -s "$PROFILE_ROOT/host.stderr" ]]; then
      cat "$PROFILE_ROOT/host.stderr" >&2
    fi
    exit 1
  fi
  sleep 0.1
done

host_version="$("$HOST_BIN" --version)"
echo "host_version=$host_version"
echo "host_bin=$HOST_BIN"
echo "cli_bin=$CLI_BIN"
echo "host_size_bytes=$(stat -c '%s' "$HOST_BIN")"
echo "cli_size_bytes=$(stat -c '%s' "$CLI_BIN")"

/usr/bin/time -f "identify_seconds=%e identify_maxrss_kb=%M" \
  "$CLI_BIN" identify >/dev/null
/usr/bin/time -f "list_seconds=%e list_maxrss_kb=%M" \
  "$CLI_BIN" list-workspaces >/dev/null

sleep "$WARMUP_SECONDS"

read -r user_ticks_start system_ticks_start < <(awk '{print $14, $15}' "/proc/$HOST_PID/stat")
rss_start="$(awk '/VmRSS/ {print $2}' "/proc/$HOST_PID/status")"
write_bytes_start="$(awk '$1=="write_bytes:" {print $2}' "/proc/$HOST_PID/io")"

rss_min="$rss_start"
rss_max="$rss_start"
sample_count=0
sample_deadline=$((SECONDS + SAMPLE_SECONDS))
while (( SECONDS < sample_deadline )); do
  sleep "$SAMPLE_INTERVAL"
  rss_sample="$(awk '/VmRSS/ {print $2}' "/proc/$HOST_PID/status")"
  if (( rss_sample < rss_min )); then
    rss_min="$rss_sample"
  fi
  if (( rss_sample > rss_max )); then
    rss_max="$rss_sample"
  fi
  sample_count=$((sample_count + 1))
done

read -r user_ticks_end system_ticks_end < <(awk '{print $14, $15}' "/proc/$HOST_PID/stat")
rss_end="$(awk '/VmRSS/ {print $2}' "/proc/$HOST_PID/status")"
write_bytes_end="$(awk '$1=="write_bytes:" {print $2}' "/proc/$HOST_PID/io")"
clock_ticks="$(getconf CLK_TCK)"
cpu_ticks_delta=$(((user_ticks_end + system_ticks_end) - (user_ticks_start + system_ticks_start)))

awk \
  -v ticks="$cpu_ticks_delta" \
  -v hz="$clock_ticks" \
  -v seconds="$SAMPLE_SECONDS" \
  'BEGIN { printf "idle_cpu_seconds_delta=%.4f\nidle_cpu_percent=%.3f\n", ticks / hz, (ticks / hz) * 100 / seconds }'

echo "idle_sample_seconds=$SAMPLE_SECONDS"
echo "idle_warmup_seconds=$WARMUP_SECONDS"
echo "idle_sample_count=$sample_count"
echo "idle_rss_kb_start=$rss_start"
echo "idle_rss_kb_min=$rss_min"
echo "idle_rss_kb_max=$rss_max"
echo "idle_rss_kb_end=$rss_end"
echo "idle_rss_kb_delta=$((rss_end - rss_start))"
echo "idle_write_bytes_delta=$((write_bytes_end - write_bytes_start))"
echo "profile_root=$PROFILE_ROOT"
