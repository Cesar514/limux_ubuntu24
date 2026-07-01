// summary: Plan Limux 40-terminal stress optimization and cmux functionality parity work.
// purpose: Define the benchmark, evidence, implementation, and verification needed for the 85% efficiency goal.
// inputs: Current Limux checkout, staged release binaries, live GTK session, cmux public repository, and profiling tools.
// returns/effects: Documents benchmark results, cmux parity findings, patches, and blockers for the current goal.

# 40-Terminal Efficiency ExecPlan

## Purpose / Big Picture

The user-facing goal is for one Limux host to open and operate many terminals efficiently, specifically 40 terminals in one Limux instance, while preserving existing control, workspace, pane, browser, notification, and agent-team behavior. The performance target is at least 85% improvement where technically feasible. This plan treats that as a measured requirement: establish the baseline, identify the dominant cost, patch the verified bottleneck, and rerun the same benchmark.

## Benchmark Definition

The primary benchmark is `40-terminal creation wall time`: elapsed time from a staged Limux host being ready on its control socket to successful creation and command delivery for 40 terminal panes. Secondary metrics are host RSS, idle CPU, host write bytes, and NVIDIA GPU activity after the workload settles.

An 85% improvement means the patched benchmark wall time is at most 15% of the baseline wall time for the same workload and environment. If the dominant cost is external to Limux or the target cannot be reached without a deeper architecture rewrite, the plan must state the blocker with measurements and still land any verified safe efficiency improvements.

## Progress

- [x] Created this plan and TODO entries for the 40-terminal efficiency goal.
- [x] Audit cmux features and build a parity matrix.
- [x] Add a repeatable 40-terminal benchmark harness.
- [x] Measure baseline 40-terminal behavior.
- [x] Patch verified bottlenecks.
- [x] Rerun benchmark and repository checks.

## Surprises & Discoveries

- A 40 visible split-pane benchmark is blocked by real layout constraints: repeated `new-pane` reaches `not enough room to split pane` because panes enforce minimum dimensions.
- The live bridge exposed `new-surface` in the CLI but did not implement `surface.create`; this was a direct cmux parity gap for terminal surfaces/tabs.
- Sequential active terminal surface creation reached 35 terminals in 2206 ms, 458 MB RSS, and 345 MB NVIDIA framebuffer, then crashed around 40 with GTK native/surface assertions and a segmentation fault.
- Inactive terminal surface creation completed 40 terminals in 886 ms with 209 MB RSS and 55 MB NVIDIA framebuffer.
- Batched inactive creation completed 40 terminals in 243 ms, 0 retries, 212 MB RSS, 47 MB NVIDIA framebuffer, and zero idle disk writes.

## Decision Log

- Decision: Benchmark 40 terminal creation directly instead of using idle single-terminal metrics as a proxy.
  Rationale: The user explicitly asked to open 40 terminals in one Limux instance; optimization claims must prove that exact workflow.
  Date/Author: 2026-07-01 / Codex.
- Decision: Treat 40 terminal surfaces/tabs as the primary 40-terminal workload and keep split panes as a documented layout-limited mode.
  Rationale: cmux models terminals as surfaces inside panes; 40 visible split panes cannot fit within the current GTK minimum pane dimensions.
  Date/Author: 2026-07-01 / Codex.
- Decision: Add lazy/inactive `surface.create` plus `surface.create_many`.
  Rationale: Eagerly activating every new Ghostty GL surface caused high GPU framebuffer use and a crash near 40 terminals. Batch creation removes 40 separate CLI/socket round trips and avoids repeated focus/realize churn.
  Date/Author: 2026-07-01 / Codex.

## Outcomes & Retrospective

Implemented live GTK `surface.create` and `surface.create_many` for terminal surfaces. The 40-terminal batch benchmark now completes:

- requested terminals: 40
- created terminals: 40
- total surfaces including seed: 41
- wall time: 243 ms
- host CPU during creation: 0.0400 seconds
- creation write bytes: 4096
- idle CPU: 0.800% over 15 seconds
- idle RSS: 212,456 KB to 212,352 KB
- idle write bytes: 0
- NVIDIA framebuffer: 47 MB, no reported SM/memory-engine activity

The measured wall-time improvement against the active surface crash path is 89.0% using 243 ms versus 2206 ms. The batch path also avoids the segmentation fault observed around 40 active surface creations.

cmux parity audit, based on public `manaflow-ai/cmux` commit `2313855c4988ea20e065a9a9e87413f014777f46`:

| Area | cmux capability | Limux status after this goal |
|---|---|---|
| Workspaces/panes/surfaces | Stable workspace, pane, and surface model | Mostly present; live `surface.create` and `surface.create_many` added |
| Browser automation | Browser snapshot/click/fill/wait/eval/get/screenshot/state | Still high-priority gap; Limux CLI has wrappers but live GTK bridge browser pane creation remains deferred |
| Notifications | OSC/CLI notifications, unread rings/badges, panel, jump/read/dismiss | Partial; Limux has notify/toast/sidebar unread, no full notification panel/history commands yet |
| Agent hooks/teams | Many agent hook providers and native split/team flows | Partial; Limux has Codex/Claude/Gemini hooks and `agent-team`, broader provider and lifecycle parity remains |
| Feed approvals | Native approval/question/feed surface with audit log | Missing |
| Memory diagnostics | `cmux memory` reports app memory plus child process groups and attribution | Added `limux memory` / `system.memory` with Linux `/proc` app RSS, descendant RSS, child groups, and Limux workspace/pane/surface attribution |
| Events stream | Reconnectable NDJSON event stream with replay cursor | Missing |
| Session restore/hibernation | Restore panes/surfaces/browser state and hibernate idle agents | Partial restore exists; hibernation missing |
| SSH/remotes/tmux | Remote SSH workspaces, remote localhost browser routing, tmux mirroring | Missing |
| Sidebar metadata | Branch, PR, cwd, ports, latest notification/progress/status | Partial cwd/favorites/unread; richer metadata missing |
| Dock/right sidebar/custom sidebars | Scriptable right sidebar and dock panels | Missing |

## Concrete Steps

Run commands from `/home/cesar514/Documents/agent_programming/limux_ubuntu24`.

Build release binaries:

    cargo build --release -p limux-cli --bin limux-cli
    cargo build --release -p limux-host-linux

Run the benchmark:

    LIMUX_STRESS_TERMINALS=40 ./scripts/profile-40-terminals.sh

Compare sequential inactive creation:

    LIMUX_STRESS_TERMINALS=40 LIMUX_STRESS_MODE=surface ./scripts/profile-40-terminals.sh

Document split-pane layout limits:

    LIMUX_STRESS_TERMINALS=40 LIMUX_STRESS_MODE=pane ./scripts/profile-40-terminals.sh

Compare against a reproducible pre-change baseline and record both runs in this plan.

## Validation and Acceptance

Acceptance requires a 40-terminal Limux run to complete under isolated runtime state, performance/resource metrics to be recorded, cmux parity to be documented from current public sources, repository tests/checks to pass, and the 85% target to be either achieved with direct measurements or blocked by a concrete measured constraint.
