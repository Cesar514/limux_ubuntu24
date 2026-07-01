// summary: Track Limux runtime performance, leak, GPU, and disk-write profiling work.
// purpose: Provide a reproducible plan to install/profile Limux and optimize verified bottlenecks.
// inputs: Local checkout, installed Limux binary, release/debug builds, Xvfb smoke tests, strace, perf, and process metrics.
// returns/effects: Documents baseline evidence, patches, and before/after verification for performance/resource goals.

# Performance and Leak Profiling ExecPlan

## Purpose / Big Picture

This plan exists because build tests alone do not prove that Limux is efficient, leak-free, low-latency, GPU-light, or free of continuous disk writes. The user-visible outcome is a Limux build that has been installed or staged in a reproducible way, exercised as a real GTK host, measured for resource behavior, and improved only where evidence identifies a bottleneck.

## Progress

- [x] (2026-07-01 11:01Z) Confirmed the system-installed `limux` is still `0.1.13`, while the repository is newer and has local safety patches.
- [x] (2026-07-01 11:03Z) Created TODO.md entries for the performance/leak goal.
- [x] Build an optimized profileable current Limux binary.
- [x] Add a repeatable live-display profiling script for disk writes, memory growth, CPU, and control latency.
- [x] Compare against a git worktree checked out at the pre-performance baseline commit `6f7cd53`.
- [x] Patch the verified Ghostty mailbox polling inefficiency.
- [x] Re-run runtime profiling and repository checks.
- [x] Document the Xvfb blocker: `xvfb-run` is not installed and `sudo -n true` reports that a password is required.

## Surprises & Discoveries

- Observation: `/usr/bin/limux` reports `Limux 0.1.13`, not the current repo version.
  Evidence: `which limux` returned `/usr/bin/limux`; `limux --version` returned `Limux 0.1.13`.
- Observation: Local tooling includes `strace`, `perf`, and `nvidia-smi`, but no `valgrind` was found in PATH.
  Evidence: `command -v strace`, `command -v perf`, and `command -v nvidia-smi` succeeded; `command -v valgrind` produced no path.
- Observation: A live X11 session is available and can run real GTK/Ghostty profiling without touching user state.
  Evidence: `scripts/profile-runtime.sh` launched staged release binaries with isolated `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_RUNTIME_DIR`, and `LIMUX_SOCKET`.
- Observation: The existing Xvfb smoke test cannot run in this environment without installing `xvfb`.
  Evidence: `./scripts/xvfb-smoke-test.sh` failed because `xvfb-run` is missing; `sudo -n true` reported that a password is required.

## Decision Log

- Decision: Do not claim memory/GPU/disk-write proof until a real host process has been measured.
  Rationale: Unit tests and clippy do not exercise GTK, Ghostty rendering, control socket traffic, or session-save behavior under runtime load.
  Date/Author: 2026-07-01 / Codex.
- Decision: Use a staged local release binary before attempting a system package replacement.
  Rationale: The environment may not allow privileged package installation, and performance evidence can be collected from exact repo-built binaries without changing the user's system package first.
  Date/Author: 2026-07-01 / Codex.
- Decision: Reduce the unconditional Ghostty mailbox timer from 8 ms to 16 ms and keep event-driven ticks through `ghostty_wakeup_cb`.
  Rationale: The runtime already coalesces Ghostty wakeups into idle ticks, so the periodic timer only needs to cap latency for unsignaled renderer messages. A 16 ms cap aligns with a 60 Hz frame budget and halves unconditional wake frequency.
  Date/Author: 2026-07-01 / Codex.

## Outcomes & Retrospective

Current Limux was built and staged from the repo release profile. A reproducible baseline was built from git commit `6f7cd53` in `/tmp/limux-baseline-profile`, linked to the same local Ghostty artifact.

Before patch, using `LIMUX_PROFILE_SAMPLE_SECONDS=60` with a 5 second warmup:

- host size: 2,767,920 bytes
- CLI `identify`: 0.02 seconds, 3,412 KB max RSS
- CLI `list-workspaces`: 0.03 seconds, 3,548 KB max RSS
- idle CPU: 0.1000 CPU seconds over 60 seconds, 0.167%
- idle host RSS: 183,696 KB start, 183,620 KB end, -76 KB delta
- idle disk writes: 0 bytes

After patch, using the same command and workload:

- host size: 2,767,824 bytes
- CLI `identify`: 0.05 seconds, 3,580 KB max RSS
- CLI `list-workspaces`: 0.02 seconds, 3,652 KB max RSS
- idle CPU: 0.0700 CPU seconds over 60 seconds, 0.117%
- idle host RSS: 183,512 KB start, 183,448 KB end, -64 KB delta
- idle disk writes: 0 bytes

The measured idle CPU reduction is 0.050 percentage points, about 30% relative reduction in idle host CPU for this workload. The patched host also remained stable in the short idle leak window: RSS decreased slightly after warmup and `/proc/<pid>/io` reported zero idle `write_bytes`.

NVIDIA GPU sampling with `nvidia-smi pmon -s um -c 10` on the patched host reported Limux as a graphics process using 8 MB framebuffer, with no reported SM or memory-engine utilization in the sampled idle rows. Other desktop processes, especially Xorg and gnome-shell, accounted for visible GPU activity during the sample.

Remaining blocker: headless Xvfb verification could not be run because `xvfb-run` is absent and installing `xvfb` requires a sudo password in this environment.

## Context and Orientation

Limux is a Rust workspace with a GTK host binary in `rust/limux-host-linux`, a CLI in `rust/limux-cli`, a control socket crate in `rust/limux-control`, and Ghostty integration through `rust/limux-ghostty-sys` plus the `ghostty` submodule. The runtime smoke test at `scripts/xvfb-smoke-test.sh` can launch a real host under Xvfb, create workspaces, send text, and exercise the control socket without physical display hardware.

For this plan, a "continuous disk write" means writes or fsync-like activity continuing while the host is idle after startup and smoke actions have finished. A "memory leak" means sustained resident-memory growth across repeated operations that does not plateau after the workload stops. "GPU leak" is harder to prove under Xvfb because the smoke test forces software rendering; if real GPU telemetry is unavailable, the plan must say so and use CPU/render-loop symptoms as weaker evidence.

## Plan of Work

First, build the current release binaries and verify they run. Next, run the smoke test with isolated `XDG_*` paths so no real user state is touched. Then profile the host process with `strace` for filesystem writes, `/proc/<pid>/status` or `ps` for resident memory, and `perf stat` or timing around CLI commands for CPU/latency. Compare those measurements to either the currently installed `0.1.13` where it can run the same scenario, or to a git worktree checked out at the pre-performance baseline commit. Patch the smallest bottleneck supported by the evidence, then re-run the same profile.

## Concrete Steps

Run all commands from `/home/cesar514/Documents/agent_programming/limux_ubuntu24`.

Build release binaries:

    cargo build --release -p limux-cli --bin limux-cli
    cargo build --release -p limux-host-linux

Run the live-display resource profile:

    LIMUX_PROFILE_SAMPLE_SECONDS=60 ./scripts/profile-runtime.sh

Run the existing smoke test:

    ./scripts/xvfb-smoke-test.sh

Profile disk writes with an isolated runtime directory:

    strace -ff -e trace=%file,write,fsync,fdatasync,rename,unlink -o /tmp/limux-strace target/release/limux

Collect memory samples while a smoke workload runs:

    while kill -0 "$HOST_PID"; do awk '/VmRSS|VmHWM/ {print strftime(), $0}' /proc/$HOST_PID/status; sleep 1; done

Record latency for common CLI calls:

    /usr/bin/time -f '%e %M' target/release/limux-cli list-workspaces

## Validation and Acceptance

Acceptance requires current Limux to be built and exercised as a real host, final repository checks to pass, and at least one performance/resource claim to have before/after evidence. If no code bottleneck can be safely patched in the available environment, acceptance requires clear profiling evidence and a documented blocker; otherwise the goal remains active.

## Idempotence and Recovery

All profiling should use temporary `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_RUNTIME_DIR`, and socket paths. The smoke test already creates a temporary demo directory and removes it on success. Profiling output under `/tmp` can be deleted after summaries are recorded. Do not overwrite `/usr/bin/limux` unless package installation is explicitly available and the built artifact has already passed smoke tests.

## Artifacts and Notes

The previous safety goal ended with `scripts/check.sh` passing and local branch pushed to `origin`. This performance goal starts from that clean state.

## Interfaces and Dependencies

Use existing project scripts and Rust tooling. Do not add new runtime dependencies for measurement. If helper scripts are added, they should live under `scripts/` and use standard Linux tools already present in this environment where possible.
