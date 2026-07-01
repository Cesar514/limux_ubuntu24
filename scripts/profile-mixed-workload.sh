#!/usr/bin/env bash
# summary: Stress profile Limux with multiple workspaces, split blocks, and distributed terminal tabs.
# purpose: Measure mixed-workload latency, host resources, idle writes, and GPU-visible process state.
# inputs: LIMUX_MIXED_* environment variables, DISPLAY, jq, awk, release host binary, and release CLI binary.
# returns/effects: Launches an isolated Limux host, creates the requested mixed workload, prints metrics, and removes temp state.

set -euo pipefail

HOST_BIN="${LIMUX_MIXED_HOST_BIN:-target/release/limux}"
CLI_BIN="${LIMUX_MIXED_CLI_BIN:-target/release/limux-cli}"
WORKSPACES="${LIMUX_MIXED_WORKSPACES:-4}"
PANES_PER_WORKSPACE="${LIMUX_MIXED_PANES_PER_WORKSPACE:-4}"
TERMINALS_PER_WORKSPACE="${LIMUX_MIXED_TERMINALS_PER_WORKSPACE:-10}"
WARMUP_SECONDS="${LIMUX_MIXED_WARMUP_SECONDS:-5}"
IDLE_SECONDS="${LIMUX_MIXED_IDLE_SECONDS:-15}"
MODE="${LIMUX_MIXED_MODE:-batch}"

if [[ ! -x "$HOST_BIN" ]]; then
  echo "FATAL: host binary is not executable: $HOST_BIN" >&2
  exit 1
fi

if [[ ! -x "$CLI_BIN" ]]; then
  echo "FATAL: CLI binary is not executable: $CLI_BIN" >&2
  exit 1
fi

if [[ -z "${DISPLAY:-}" ]]; then
  echo "FATAL: DISPLAY is required for GTK mixed-workload profiling" >&2
  exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "FATAL: jq is required for mixed-workload profiling" >&2
  exit 1
fi

if ! command -v awk >/dev/null 2>&1; then
  echo "FATAL: awk is required for mixed-workload profiling" >&2
  exit 1
fi

if (( WORKSPACES < 1 )); then
  echo "FATAL: LIMUX_MIXED_WORKSPACES must be at least 1" >&2
  exit 1
fi

if (( PANES_PER_WORKSPACE < 1 )); then
  echo "FATAL: LIMUX_MIXED_PANES_PER_WORKSPACE must be at least 1" >&2
  exit 1
fi

if (( TERMINALS_PER_WORKSPACE < PANES_PER_WORKSPACE )); then
  echo "FATAL: LIMUX_MIXED_TERMINALS_PER_WORKSPACE must be >= LIMUX_MIXED_PANES_PER_WORKSPACE" >&2
  exit 1
fi

if [[ "$MODE" != "batch" && "$MODE" != "sequential" ]]; then
  echo "FATAL: LIMUX_MIXED_MODE must be batch or sequential" >&2
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
    cat "$PROFILE_ROOT/host.stderr" >&2
    exit 1
  fi
  if (( SECONDS >= socket_deadline )); then
    echo "FATAL: Limux host did not create control socket within 15 seconds" >&2
    cat "$PROFILE_ROOT/host.stderr" >&2
    exit 1
  fi
  sleep 0.1
done

request_json() {
  "$CLI_BIN" --json --request "$1"
}

create_workspace() {
  local index="$1"
  local request
  if [[ "$MODE" == "batch" ]]; then
    request="$(jq -cn \
      --arg cwd "$PWD" \
      --arg name "mixed-$index" \
      '{method:"workspace.create",params:{cwd:$cwd,name:$name}}')"
  else
    local command="printf 'limux-mixed-w${index}-seed-ready\n'; sleep 3600"
    request="$(jq -cn \
      --arg cwd "$PWD" \
      --arg name "mixed-$index" \
      --arg command "$command" \
      '{method:"workspace.create",params:{cwd:$cwd,name:$name,command:$command}}')"
  fi
  request_json "$request"
}

workspace_id_from_json() {
  jq -r '.workspace_id // .workspace_ref // .workspace.workspace_id // .workspace.workspace_ref // .workspace.ref // empty'
}

pane_ids_for_workspace() {
  local workspace="$1"
  local deadline=$((SECONDS + 10))
  while true; do
    local output
    output="$("$CLI_BIN" --json list-panes --workspace "$workspace" 2>/dev/null || true)"
    if [[ -n "$output" ]]; then
      jq -r '.panes[] | .pane_id // .pane_ref' <<<"$output"
      return
    fi
    if (( SECONDS >= deadline )); then
      echo "FATAL: failed to list panes for workspace $workspace within 10 seconds" >&2
      exit 1
    fi
    sleep 0.1
  done
}

wait_for_pane_count() {
  local workspace="$1"
  local expected="$2"
  local deadline=$((SECONDS + 10))
  while true; do
    local count
    count="$("$CLI_BIN" --json list-panes --workspace "$workspace" 2>/dev/null | jq '.panes | length' 2>/dev/null || true)"
    if [[ -z "$count" ]]; then
      count=0
    fi
    if (( count >= expected )); then
      return
    fi
    if (( SECONDS >= deadline )); then
      echo "FATAL: workspace $workspace did not reach $expected panes within 10 seconds" >&2
      exit 1
    fi
    sleep 0.1
  done
}

sequential_create_panes() {
  local workspace="$1"
  local count="$2"
  local command_mode="${3:-with-command}"
  local parent_pane=""
  wait_for_pane_count "$workspace" 1
  parent_pane="$("$CLI_BIN" --json list-panes --workspace "$workspace" | jq -r '.panes[0].pane_id // .panes[0].pane_ref')"
  for i in $(seq 1 "$count"); do
    local direction="right"
    if (( i % 2 == 0 )); then
      direction="down"
    fi
    local command="printf 'limux-mixed-pane-$i-ready\n'; sleep 3600"
    local created_json
    if [[ "$command_mode" == "with-command" ]]; then
      created_json="$("$CLI_BIN" --json new-pane \
        --workspace "$workspace" \
        --pane "$parent_pane" \
        --direction "$direction" \
        --command "$command")"
    else
      created_json="$("$CLI_BIN" --json new-pane \
        --workspace "$workspace" \
        --pane "$parent_pane" \
        --direction "$direction")"
    fi
    parent_pane="$(jq -r '.pane_id // .pane_ref // empty' <<<"$created_json")"
    parent_pane="${parent_pane#pane:}"
    if [[ -z "$parent_pane" ]]; then
      echo "FATAL: sequential new-pane response missing pane_id" >&2
      echo "$created_json" >&2
      exit 1
    fi
    if [[ "$command_mode" == "with-command" ]]; then
      wait_for_pane_count "$workspace" "$((i + 1))"
    elif [[ -n "${LIMUX_MIXED_SPLIT_TICK_SECONDS:-}" ]]; then
      sleep "$LIMUX_MIXED_SPLIT_TICK_SECONDS"
    else
      wait_for_pane_count "$workspace" "$((i + 1))"
    fi
  done
}

batch_distribute_extra_surfaces() {
  local workspace="$1"
  local extra="$2"
  if (( extra < 1 )); then
    return
  fi
  mapfile -t panes < <(pane_ids_for_workspace "$workspace")
  if (( ${#panes[@]} == 0 )); then
    echo "FATAL: workspace has no panes for distributed surface creation: $workspace" >&2
    exit 1
  fi
  if [[ "${LIMUX_MIXED_DISTRIBUTE_SURFACES:-0}" != "1" ]]; then
    local command_template="printf 'limux-mixed-tab-{i}-ready\n'; sleep 3600"
    local request
    request="$(jq -cn \
      --arg workspace "$workspace" \
      --arg pane_id "${panes[0]}" \
      --argjson count "$extra" \
      --arg command_template "$command_template" \
      '{method:"surface.create_many",params:{workspace_id:$workspace,pane_id:$pane_id,count:$count,command_template:$command_template}}')"
    request_json "$request" >/dev/null
    return
  fi
  for index in "${!panes[@]}"; do
    local pane_extra=$((extra / ${#panes[@]}))
    if (( index < extra % ${#panes[@]} )); then
      pane_extra=$((pane_extra + 1))
    fi
    if (( pane_extra == 0 )); then
      continue
    fi
    local command_template="printf 'limux-mixed-tab-{i}-ready\n'; sleep 3600"
    local request
    request="$(jq -cn \
      --arg workspace "$workspace" \
      --arg pane_id "${panes[$index]}" \
      --argjson count "$pane_extra" \
      --arg command_template "$command_template" \
      '{method:"surface.create_many",params:{workspace_id:$workspace,pane_id:$pane_id,count:$count,command_template:$command_template}}')"
    request_json "$request" >/dev/null
  done
}

sequential_create_extra_surfaces() {
  local workspace="$1"
  local extra="$2"
  for i in $(seq 1 "$extra"); do
    local command="printf 'limux-mixed-tab-$i-ready\n'; sleep 3600"
    "$CLI_BIN" --json new-surface --workspace "$workspace" --command "$command" >/dev/null
  done
}

batch_create_workspaces() {
  local request
  request="$(jq -cn \
    --arg cwd "$PWD" \
    --arg name_prefix "mixed" \
    --argjson count "$WORKSPACES" \
    --argjson panes "$PANES_PER_WORKSPACE" \
    --argjson terminals "$TERMINALS_PER_WORKSPACE" \
    '{method:"workspace.create_many",params:{cwd:$cwd,name_prefix:$name_prefix,count:$count,panes_per_workspace:$panes,terminals_per_workspace:$terminals}}')"
  request_json "$request"
}

read -r create_user_ticks_start create_system_ticks_start < <(awk '{print $14, $15}' "/proc/$HOST_PID/stat")
write_bytes_create_start="$(awk '$1=="write_bytes:" {print $2}' "/proc/$HOST_PID/io")"
create_start_ns="$(date +%s%N)"

created_workspaces=0
created_panes=0
created_surfaces=0
workspace_ids=()
if [[ "$MODE" == "batch" && "${LIMUX_MIXED_LAYOUT_BATCH:-1}" == "1" ]]; then
  workspace_json="$(batch_create_workspaces)"
  mapfile -t workspace_ids < <(jq -r '.workspaces[] | .workspace_id // .workspace_ref // .workspace.workspace_id // .workspace.workspace_ref // .workspace.ref' <<<"$workspace_json")
  created_workspaces="$(jq -r '.count' <<<"$workspace_json")"
  created_panes=$((created_workspaces * PANES_PER_WORKSPACE))
  created_surfaces=$((created_workspaces * TERMINALS_PER_WORKSPACE))
else
  for workspace_index in $(seq 1 "$WORKSPACES"); do
    workspace_json="$(create_workspace "$workspace_index")"
    workspace="$(workspace_id_from_json <<<"$workspace_json")"
    if [[ -z "$workspace" ]]; then
      echo "FATAL: workspace.create did not return a workspace id/ref" >&2
      echo "$workspace_json" >&2
      exit 1
    fi
    workspace_ids+=("$workspace")
    created_workspaces=$((created_workspaces + 1))
    created_panes=$((created_panes + 1))
    created_surfaces=$((created_surfaces + 1))

    pane_splits=$((PANES_PER_WORKSPACE - 1))
    sequential_create_panes "$workspace" "$pane_splits"
    created_panes=$((created_panes + pane_splits))
    created_surfaces=$((created_surfaces + pane_splits))

    extra_surfaces=$((TERMINALS_PER_WORKSPACE - PANES_PER_WORKSPACE))
    sequential_create_extra_surfaces "$workspace" "$extra_surfaces"
    created_surfaces=$((created_surfaces + extra_surfaces))
  done
fi

create_end_ns="$(date +%s%N)"
read -r create_user_ticks_end create_system_ticks_end < <(awk '{print $14, $15}' "/proc/$HOST_PID/stat")
write_bytes_create_end="$(awk '$1=="write_bytes:" {print $2}' "/proc/$HOST_PID/io")"

sleep "$WARMUP_SECONDS"

observed_panes=0
observed_surfaces=0
for workspace in "${workspace_ids[@]}"; do
  panes_json="$("$CLI_BIN" --json list-panes --workspace "$workspace")"
  surfaces_json="$("$CLI_BIN" --json list-panels --workspace "$workspace")"
  observed_panes=$((observed_panes + $(jq '.panes | length' <<<"$panes_json")))
  observed_surfaces=$((observed_surfaces + $(jq '.surfaces | length' <<<"$surfaces_json")))
done

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
echo "mixed_mode=$MODE"
echo "requested_workspaces=$WORKSPACES"
echo "requested_panes_per_workspace=$PANES_PER_WORKSPACE"
echo "requested_terminals_per_workspace=$TERMINALS_PER_WORKSPACE"
echo "requested_total_terminals=$((WORKSPACES * TERMINALS_PER_WORKSPACE))"
echo "created_workspaces=$created_workspaces"
echo "created_panes=$created_panes"
echo "created_surfaces=$created_surfaces"
echo "observed_panes=$observed_panes"
echo "observed_surfaces=$observed_surfaces"
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
