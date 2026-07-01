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
