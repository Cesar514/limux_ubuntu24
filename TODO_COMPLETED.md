// Completed tasks for the current goal. \
// Format: [x] <timestamp> <task> \
// Example: [x] 2026-04-05T20:39:27Z Verify the completed-task log is sorted newest-first using 24-hour HH:MM:SS UTC timestamps. \

[x] 2026-07-01T12:59:37Z Move completed mixed-workload TODO entries to TODO_COMPLETED.md, commit the verified changes, and push only to the fork remote. \
[x] 2026-07-01T12:59:37Z Re-run repository checks and the mixed-workload profiler to verify stability and resource behavior. \
[x] 2026-07-01T12:59:37Z Patch the verified mixed-workload bottleneck without removing existing Limux functionality. \
[x] 2026-07-01T12:59:37Z Measure the existing sequential multi-workspace split-pane path and identify the dominant bottleneck. \
[x] 2026-07-01T12:59:37Z Add a repeatable mixed-workload profiler that measures creation latency, host CPU, RSS, disk writes, GPU state, pane count, surface count, and workspace count. \
[x] 2026-07-01T12:59:37Z Define and document a mixed Limux workload with multiple workspaces, multiple split panes per workspace, and multiple terminal surfaces per workspace. \
[x] 2026-07-01T12:19:05Z Run repository verification and move completed 40-terminal efficiency TODO entries to TODO_COMPLETED.md. \
[x] 2026-07-01T12:19:05Z Migrate high-value cmux surface/tab functionality into Limux with live surface.create and surface.create_many. \
[x] 2026-07-01T12:19:05Z Re-run the 40-terminal benchmark and prove the 85% target with measured batch creation results. \
[x] 2026-07-01T12:19:05Z Patch verified 40-terminal inefficiencies while preserving existing Limux functionality. \
[x] 2026-07-01T12:19:05Z Identify eager activation and repeated CLI/socket calls as dominant 40-terminal bottlenecks. \
[x] 2026-07-01T12:19:05Z Capture baseline 40-terminal latency, host CPU, RSS, GPU, and disk-write behavior. \
[x] 2026-07-01T12:19:05Z Build a repeatable 40-terminal Limux stress benchmark with isolated runtime state. \
[x] 2026-07-01T12:19:05Z Audit current cmux functionality and map it against Limux features. \
[x] 2026-07-01T12:19:05Z Create an 85%-efficiency 40-terminal stress ExecPlan with benchmark definition and acceptance criteria. \
[x] 2026-07-01T11:18:21Z Move completed performance-goal items to TODO_COMPLETED.md and leave TODO.md clean. \
[x] 2026-07-01T11:18:21Z Re-run profiling and tests to prove measurable improvements and document the Xvfb privilege blocker. \
[x] 2026-07-01T11:18:21Z Patch the verified Ghostty mailbox polling inefficiency while preserving functionality and hard-cutout behavior. \
[x] 2026-07-01T11:18:21Z Identify Ghostty's unconditional 8 ms mailbox timer as the measured runtime hotspot. \
[x] 2026-07-01T11:18:21Z Compare patched Limux measurements against a reproducible pre-performance git baseline. \
[x] 2026-07-01T11:18:21Z Run live-display runtime profiling for disk writes, memory growth, CPU, and NVIDIA GPU behavior. \
[x] 2026-07-01T11:18:21Z Establish the installed/current-version baseline and build profileable optimized Limux binaries. \
[x] 2026-07-01T11:02:53Z Create a reproducible performance and leak profiling plan for Limux. \
[x] 2026-07-01T10:54:59Z Move completed work items to TODO_COMPLETED.md and leave TODO.md clean. \
[x] 2026-07-01T10:54:59Z Verify the patched behavior with the strongest local checks available. \
[x] 2026-07-01T10:54:59Z Compare Limux with similar terminal multiplexers and document practical missing functions. \
[x] 2026-07-01T10:54:59Z Evaluate whether a language conversion is useful now and keep existing functionality intact. \
[x] 2026-07-01T10:54:59Z Patch validated safety issues without adding silent fallbacks. \
[x] 2026-07-01T10:54:59Z Audit high-risk leak, socket, filesystem, process, and terminal-control paths that could expose data or damage user files. \
[x] 2026-07-01T10:46:00Z Baseline the project build/test state after updating from upstream. \
[x] 2026-07-01T10:45:49Z Fetch and integrate the latest upstream limux changes without discarding local fork work. \
[x] 2026-04-30T08:36:47Z Verify Limux loads and renders the recovered workspace list through the control socket. \
[x] 2026-04-30T08:36:47Z Apply the session recovery and GTK CSS parser fixes without adding silent runtime fallbacks. \
[x] 2026-04-30T08:36:47Z Identify that an empty canonical session.json was masking the non-empty legacy workspaces.json file. \
[x] 2026-04-30T08:36:47Z Reproduce the Limux startup and empty workspace/session state on this machine. \
[x] 2026-04-15T20:42:40Z Move completed tasks into TODO_COMPLETED.md and push the git change to the user's fork. \
[x] 2026-04-15T20:41:49Z Verify that `limux` now resolves to the updated installed version rather than the old `0.1.12` package. \
[x] 2026-04-15T20:41:49Z Upgrade the installed Debian Limux package on this Ubuntu 24.04 machine to the freshly built `0.1.13` artifact from this repo. \
[x] 2026-04-15T20:41:49Z Ignore `.scplus/` in the repo and prepare the git change for commit. \
[x] 2026-04-15T20:36:22Z Move completed TODO items into TODO_COMPLETED.md, leave only unfinished work in TODO.md, and commit and push the verified changes to the user's fork. \
[x] 2026-04-15T20:34:21Z Verify the requested outcome directly on this Ubuntu 24.04 environment by running the new bootstrap script and the repository quality/build checks that exercise the exact flow. \
[x] 2026-04-15T20:34:21Z Update repository build/package/docs wiring so the scripted flow succeeds on Ubuntu 24.04 without hidden manual steps. \
[x] 2026-04-15T20:34:21Z Add a single Ubuntu 24.04 bootstrap/build shell script that installs or provisions the required toolchain and runs the full build/package flow from a fresh checkout. \
[x] 2026-04-15T20:34:21Z Reproduce the current Ubuntu 24.04 build and packaging path, document concrete blockers, and identify the minimum repo changes required for compatibility. \
