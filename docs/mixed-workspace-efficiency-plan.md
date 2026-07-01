// summary: Document mixed workspace, pane subdivision, and terminal-surface profiling.
// purpose: Capture benchmark shape, optimization decisions, measured results, and GTK split batching blockers.
// inputs: Live GTK Limux host, release CLI/host binaries, scripts/profile-mixed-workload.sh, and NVIDIA process samples.
// returns/effects: Provides reproducible evidence for mixed-workload efficiency and remaining split-pane constraints.

# Mixed Workspace Efficiency Plan

## Goal

Verify and optimize Limux beyond the single-workspace 40-terminal case by creating multiple workspaces, split-pane subdivision blocks, and multiple terminals per workspace in one live Limux host.

## Benchmark Shape

The default mixed profile uses:

- 4 workspaces
- 4 panes per workspace
- 10 terminals per workspace
- 40 total terminals
- 16 total split panes

The baseline mode creates split panes sequentially and injects a long-running command into every structural pane and extra terminal tab.

The optimized mode keeps structural pane creation on the proven safe sequential split path, avoids unnecessary command injection for split panes, and uses targeted `surface.create_many` calls to distribute extra terminal tabs across panes.

## Results

Sequential baseline:

- created workspaces: 4
- observed panes: 16
- observed surfaces: 40
- creation wall time: 5127 ms
- host CPU during creation: 2.2300 seconds
- creation write bytes: 630784
- idle CPU: 0.333%
- idle write bytes: 0
- NVIDIA framebuffer: 206 MB

Optimized mixed mode:

- created workspaces: 4
- observed panes: 16
- observed surfaces: 40
- creation wall time: 2729 ms
- host CPU during creation: 0.9200 seconds
- creation write bytes: 557056
- idle CPU: 0.267%
- idle write bytes: 0
- NVIDIA framebuffer: 226 MB

Measured improvement:

- creation wall time: 46.8% faster
- host CPU during creation: 58.7% lower
- creation write bytes: 11.7% lower
- idle disk writes: unchanged at 0

## Split Batching Decision

Direct multi-split batching through GTK is blocked for now. Repeated immediate pane reparenting can trigger GTK native/surface assertions and a host segmentation fault while Ghostty GL areas are being unrealized and realized. The control bridge therefore rejects unsafe `pane.create_many` counts above 1 instead of exposing a crash path.

The safe optimization for this pass is to keep structural pane subdivisions on the stable sequential path and remove avoidable command readiness waits from the optimized mixed workload. Terminal-surface expansion remains batched through `surface.create_many`, including pane-targeted distribution.

## Verification Commands

Run the baseline:

    LIMUX_MIXED_MODE=sequential LIMUX_MIXED_WORKSPACES=4 LIMUX_MIXED_PANES_PER_WORKSPACE=4 LIMUX_MIXED_TERMINALS_PER_WORKSPACE=10 ./scripts/profile-mixed-workload.sh

Run the optimized mixed profile:

    LIMUX_MIXED_MODE=batch LIMUX_MIXED_WORKSPACES=4 LIMUX_MIXED_PANES_PER_WORKSPACE=4 LIMUX_MIXED_TERMINALS_PER_WORKSPACE=10 ./scripts/profile-mixed-workload.sh

## Triple Workload Result

The triple-size benchmark uses:

- 12 workspaces
- 4 panes per workspace
- 10 terminals per workspace
- 48 total split panes
- 120 total terminal surfaces

Current sequential baseline:

- observed panes: 48
- observed surfaces: 120
- creation wall time: 8373 ms
- host CPU during creation: 3.0700 seconds
- creation write bytes: 5230592
- idle CPU: 0.400%
- idle write bytes: 0
- NVIDIA framebuffer: 492 MB

Optimized layout-batched mode:

- observed panes: 48
- observed surfaces: 120
- creation wall time: 946 ms
- host CPU during creation: 0.1900 seconds
- creation write bytes: 0
- idle CPU: 0.333%
- idle write bytes: 0
- NVIDIA framebuffer: 43 MB

Measured triple-workload improvement:

- creation wall time: 88.7% faster
- host CPU during creation: 93.8% lower
- creation write bytes: eliminated for the creation window
- idle disk writes: unchanged at 0

The optimized path uses `workspace.create_many` to build the requested workspace/pane/tab layout directly in host state and activates only the final workspace once. It also avoids activating every restored terminal tab; only the saved active tab is focused during pane restoration.

Run the triple baseline:

    LIMUX_MIXED_MODE=sequential LIMUX_MIXED_WORKSPACES=12 LIMUX_MIXED_PANES_PER_WORKSPACE=4 LIMUX_MIXED_TERMINALS_PER_WORKSPACE=10 ./scripts/profile-mixed-workload.sh

Run the optimized triple profile:

    LIMUX_MIXED_MODE=batch LIMUX_MIXED_WORKSPACES=12 LIMUX_MIXED_PANES_PER_WORKSPACE=4 LIMUX_MIXED_TERMINALS_PER_WORKSPACE=10 ./scripts/profile-mixed-workload.sh

## Sustained Active-Output Result

The active-output benchmark keeps the optimized triple layout open and sends a synchronized shell loop to every terminal surface:

- 12 workspaces
- 48 panes
- 120 terminal surfaces
- 1000 printed lines per terminal
- 0.02 seconds between printed lines per terminal
- 15-second active sample window
- 10-second drain before post-activity idle sampling
- verified `read-screen` sample containing `limux-active-` output

The profiler samples host-only CPU/RSS/write bytes and process-tree CPU/RSS/write bytes. The process-tree numbers include the Limux host plus terminal child processes, so the active result reflects terminals running commands rather than just opening surfaces.
It also emits a `memory_diagnostic` block from `limux memory --json`, matching CMUX's child-process memory inspection workflow on Linux.

Old continuous GLArea auto-render mode:

- active surfaces reached: 120
- activity readback sample: ok
- active host CPU: 0.267%
- active process-tree CPU: 0.267%
- active host write bytes: 0
- active process-tree write bytes: 0
- idle host CPU after activity: 0.333%
- idle process-tree CPU after activity: 0.333%
- idle host/process-tree write bytes: 0
- NVIDIA framebuffer: 36 MB

Optimized explicit-render mode:

- active surfaces reached: 120
- activity readback sample: ok
- active host CPU: 0.333%
- active process-tree CPU: 0.333%
- active host write bytes: 0
- active process-tree write bytes: 0
- idle host CPU after activity: 0.200%
- idle process-tree CPU after activity: 0.200%
- idle host/process-tree write bytes: 0
- NVIDIA framebuffer: 35 MB

Measured sustained-usage change:

- terminal targeting correctness: fixed for inactive tab surfaces, enabling commands and readback on hidden terminal tabs
- idle CPU after active output: 40.0% lower in this sample
- NVIDIA framebuffer: 2.8% lower in this sample
- disk writes: unchanged at 0 during active output and post-activity idle

This result does not prove an 80% sustained-runtime efficiency gain. The strongest proven runtime result is that all 120 terminals can be targeted, run output, be read back, and hold zero disk writes while explicit render queueing reduces idle repaint work. The earlier 88.7% improvement applies to triple-workload creation latency, not sustained command execution.

Follow-up optimization pass:

- active surfaces reached: 120
- activity readback sample: ok
- active host CPU: 0.133%
- active process-tree CPU: 0.133%
- active host/process-tree write bytes: 0
- post-drain idle host CPU: 0.133%
- post-drain idle process-tree CPU: 0.133%
- post-drain idle RSS delta: 0 KB
- post-drain idle host/process-tree write bytes: 0
- NVIDIA framebuffer: 43 MB
- memory diagnostic: app RSS 254.4 MiB, child RSS 89.4 MiB across 16 processes

This pass also skips no-op terminal title and PWD callbacks before they relabel tabs or request session persistence. The drained idle sample is the stronger post-activity proof: after output finishes, Limux showed no continuing RSS growth and no disk writes.

Run the active-output triple profile:

    LIMUX_MIXED_MODE=batch LIMUX_MIXED_ACTIVITY=echo LIMUX_MIXED_ACTIVITY_SECONDS=15 LIMUX_MIXED_WORKSPACES=12 LIMUX_MIXED_PANES_PER_WORKSPACE=4 LIMUX_MIXED_TERMINALS_PER_WORKSPACE=10 ./scripts/profile-mixed-workload.sh
