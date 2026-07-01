#!/usr/bin/env bash
# summary: Stress profile Limux by opening many terminal panes in one host.
# purpose: Measure 40-terminal creation latency, host resources, idle writes, and GPU-visible process state.
# inputs: LIMUX_STRESS_TERMINALS, LIMUX_STRESS_HOST_BIN, LIMUX_STRESS_CLI_BIN, DISPLAY, and release binaries.
# returns/effects: Launches an isolated Limux host, creates terminal panes, prints key=value metrics, and removes temp state.

set -euo pipefail

HOST_BIN="${LIMUX_STRESS_HOST_BIN:-target/release/limux}"
CLI_BIN="${LIMUX_STRESS_CLI_BIN:-target/release/limux-cli}"
TERMINALS="${LIMUX_STRESS_TERMINALS:-40}"
WARMUP_SECONDS="${LIMUX_STRESS_WARMUP_SECONDS:-5}"
IDLE_SECONDS="${LIMUX_STRESS_IDLE_SECONDS:-15}"
MODE="${LIMUX_STRESS_MODE:-batch}"

if [[ ! -x "$HOST_BIN" ]]; then
  echo "FATAL: host binary is not executable: $HOST_BIN" >&2
  exit 1
fi

if [[ ! -x "$CLI_BIN" ]]; then
  echo "FATAL: CLI binary is not executable: $CLI_BIN" >&2
  exit 1
fi

if [[ -z "${DISPLAY:-}" ]]; then
  echo "FATAL: DISPLAY is required for GTK stress profiling" >&2
  exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "FATAL: jq is required for stress profiling" >&2
  exit 1
fi

if ! command -v awk >/dev/null 2>&1; then
  echo "FATAL: awk is required for stress profiling" >&2
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

socket_deadline=$((SECONDS + 15))
while [[ ! -S "$LIMUX_SOCKET" ]]; do
  if ! kill -0 "$HOST_PID" >/dev/null 2>&1; then
    echo "FATAL: Limux host exited before creating control socket" >&2
    if [[ -s "$PROFILE_ROOT/host.stderr" ]]; then
      cat "$PROFILE_ROOT/host.stderr" >&2
    fi
    exit 1
  fi
  if (( SECONDS >= socket_deadline )); then
    echo "FATAL: Limux host did not create control socket within 15 seconds" >&2
    if [[ -s "$PROFILE_ROOT/host.stderr" ]]; then
      cat "$PROFILE_ROOT/host.stderr" >&2
    fi
    exit 1
  fi
  sleep 0.1
done

seed_command="printf 'limux-stress-seed-ready\n'; sleep 3600"
seed_request="$(jq -cn \
  --arg cwd "$PWD" \
  --arg command "$seed_command" \
  '{method:"workspace.create",params:{cwd:$cwd,command:$command}}')"
seed_json="$("$CLI_BIN" --json --request "$seed_request")"
workspace="$(jq -r '.workspace_id // .workspace_ref // .workspace.workspace_id // .workspace.workspace_ref // .workspace.ref // empty' <<<"$seed_json")"
if [[ -z "$workspace" ]]; then
  echo "FATAL: workspace.create did not return a workspace id/ref" >&2
  echo "$seed_json" >&2
  exit 1
fi

surface_deadline=$((SECONDS + 10))
surface=""
while [[ -z "$surface" ]]; do
  surfaces_json="$("$CLI_BIN" --json list-panels --workspace "$workspace")"
  surface="$(jq -r '.surfaces[0].surface_id // .surfaces[0].surface_ref // empty' <<<"$surfaces_json")"
  if [[ -n "$surface" ]]; then
    break
  fi
  if (( SECONDS >= surface_deadline )); then
    echo "FATAL: seed workspace did not expose a terminal surface within 10 seconds" >&2
    echo "$surfaces_json" >&2
    exit 1
  fi
  sleep 0.1
done

read -r create_user_ticks_start create_system_ticks_start < <(awk '{print $14, $15}' "/proc/$HOST_PID/stat")
write_bytes_create_start="$(awk '$1=="write_bytes:" {print $2}' "/proc/$HOST_PID/io")"
create_start_ns="$(date +%s%N)"

created=0
create_retries=0
parent_surface="$surface"
if [[ "$MODE" == "batch" ]]; then
  command_template="printf 'limux-stress-{i}-ready\n'; sleep 3600"
  batch_request="$(jq -cn \
    --arg workspace "$workspace" \
    --argjson count "$TERMINALS" \
    --arg command_template "$command_template" \
    '{method:"surface.create_many",params:{workspace_id:$workspace,count:$count,command_template:$command_template}}')"
  batch_json="$("$CLI_BIN" --json --request "$batch_request")"
  created="$(jq -r '.count // 0' <<<"$batch_json")"
  parent_surface="$(jq -r '.surfaces[-1].surface_id // .surfaces[-1].surface_ref // empty' <<<"$batch_json")"
fi

if [[ "$MODE" != "batch" ]]; then
for i in $(seq 1 "$TERMINALS"); do
  command="printf 'limux-stress-$i-ready\n'; sleep 3600"
  created_json=""
  for attempt in 1 2 3; do
    if [[ "$MODE" == "surface" ]]; then
      if created_json="$("$CLI_BIN" --json new-surface \
        --workspace "$workspace" \
        --command "$command" 2>"$PROFILE_ROOT/create-$i-$attempt.err")"; then
        break
      fi
    elif [[ "$MODE" == "pane" ]]; then
      direction="down"
      if [[ "$i" == "1" ]]; then
        direction="right"
      fi
      if created_json="$("$CLI_BIN" --json new-pane \
        --workspace "$workspace" \
        --surface "$parent_surface" \
        --direction "$direction" \
        --command "$command" 2>"$PROFILE_ROOT/create-$i-$attempt.err")"; then
        break
      fi
    else
      echo "FATAL: LIMUX_STRESS_MODE must be batch, surface, or pane" >&2
      exit 1
    fi
    create_retries=$((create_retries + 1))
    if [[ "$attempt" == "3" ]]; then
      echo "FATAL: terminal creation failed at iteration $i after $attempt attempts" >&2
      cat "$PROFILE_ROOT/create-$i-$attempt.err" >&2
      if [[ -s "$PROFILE_ROOT/host.stderr" ]]; then
        cat "$PROFILE_ROOT/host.stderr" >&2
      fi
      exit 1
    fi
    sleep 0.2
  done
  parent_surface="$(jq -r '.surface_id // .surface_ref // empty' <<<"$created_json")"
  if [[ -z "$parent_surface" ]]; then
    echo "FATAL: new-pane response missing surface_id at iteration $i" >&2
    echo "$created_json" >&2
    exit 1
  fi
  created=$((created + 1))
done
fi

create_end_ns="$(date +%s%N)"
read -r create_user_ticks_end create_system_ticks_end < <(awk '{print $14, $15}' "/proc/$HOST_PID/stat")
write_bytes_create_end="$(awk '$1=="write_bytes:" {print $2}' "/proc/$HOST_PID/io")"

sleep "$WARMUP_SECONDS"

panes_json="$("$CLI_BIN" --json list-panes --workspace "$workspace")"
surfaces_json="$("$CLI_BIN" --json list-panels --workspace "$workspace")"
pane_count="$(jq '.panes | length' <<<"$panes_json")"
surface_count="$(jq '.surfaces | length' <<<"$surfaces_json")"

read -r idle_user_ticks_start idle_system_ticks_start < <(awk '{print $14, $15}' "/proc/$HOST_PID/stat")
idle_rss_start="$(awk '/VmRSS/ {print $2}' "/proc/$HOST_PID/status")"
idle_write_start="$(awk '$1=="write_bytes:" {print $2}' "/proc/$HOST_PID/io")"
sleep "$IDLE_SECONDS"
read -r idle_user_ticks_end idle_system_ticks_end < <(awk '{print $14, $15}' "/proc/$HOST_PID/stat")
idle_rss_end="$(awk '/VmRSS/ {print $2}' "/proc/$HOST_PID/status")"
idle_write_end="$(awk '$1=="write_bytes:" {print $2}' "/proc/$HOST_PID/io")"

clock_ticks="$(getconf CLK_TCK)"
create_ticks_delta=$(((create_user_ticks_end + create_system_ticks_end) - (create_user_ticks_start + create_system_ticks_start)))
idle_ticks_delta=$(((idle_user_ticks_end + idle_system_ticks_end) - (idle_user_ticks_start + idle_system_ticks_start)))
create_ms=$(((create_end_ns - create_start_ns) / 1000000))

echo "host_version=$("$HOST_BIN" --version)"
echo "host_pid=$HOST_PID"
echo "host_bin=$HOST_BIN"
echo "cli_bin=$CLI_BIN"
echo "requested_terminals=$TERMINALS"
echo "stress_mode=$MODE"
echo "created_terminals=$created"
echo "create_retries=$create_retries"
echo "workspace_id=$workspace"
echo "pane_count=$pane_count"
echo "surface_count=$surface_count"
echo "create_wall_ms=$create_ms"
awk -v ticks="$create_ticks_delta" -v hz="$clock_ticks" \
  'BEGIN { printf "create_host_cpu_seconds=%.4f\n", ticks / hz }'
echo "create_write_bytes_delta=$((write_bytes_create_end - write_bytes_create_start))"
echo "idle_sample_seconds=$IDLE_SECONDS"
awk -v ticks="$idle_ticks_delta" -v hz="$clock_ticks" -v seconds="$IDLE_SECONDS" \
  'BEGIN { printf "idle_cpu_seconds_delta=%.4f\nidle_cpu_percent=%.3f\n", ticks / hz, (ticks / hz) * 100 / seconds }'
echo "idle_rss_kb_start=$idle_rss_start"
echo "idle_rss_kb_end=$idle_rss_end"
echo "idle_rss_kb_delta=$((idle_rss_end - idle_rss_start))"
echo "idle_write_bytes_delta=$((idle_write_end - idle_write_start))"

if command -v nvidia-smi >/dev/null 2>&1; then
  echo "gpu_pmon_sample_begin"
  nvidia-smi pmon -s um -c 2 | awk -v pid="$HOST_PID" '$2 == pid || /^#/'
  echo "gpu_pmon_sample_end"
fi

echo "profile_root=$PROFILE_ROOT"
