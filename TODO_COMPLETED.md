// Completed tasks for the current goal. \
// Format: [x] <timestamp> <task> \
// Example: [x] 2026-04-05T20:39:27Z Verify the completed-task log is sorted newest-first using 24-hour HH:MM:SS UTC timestamps. \

[x] 2026-07-02T06:03:00Z Implement and verify direct right-sidebar Feed exit-plan and question controls for pending CMUX requests. \
[x] 2026-07-02T05:57:12Z Implement and verify direct right-sidebar Feed permission controls for pending Codex/CMUX requests. \
[x] 2026-07-02T05:51:07Z Implement and verify CMUX `feed tui [--opentui|--legacy]` routing to Limux's live Feed right-sidebar UI surface. \
[x] 2026-07-02T05:47:22Z Implement and verify CMUX `feed clear [--yes|-y]` local workstream history deletion plus live `feed.clear` retained Feed clearing. \
[x] 2026-07-02T05:40:41Z Implement and verify explicit CMUX top-level `feed tui` and `feed clear` `not_supported` surfacing while preserving `feed --help` and `hooks feed` behavior. \
[x] 2026-07-02T05:38:17Z Implement and verify explicit CMUX `remote`/`remotes` cloud device-registry command `not_supported` surfacing while preserving no-socket help probes. \
[x] 2026-07-02T05:31:48Z Implement and verify CMUX-compatible socket password auth semantics for CLI `--password`, `CMUX_SOCKET_PASSWORD`, password file lookup, password socket mode, JSON `auth.login`, and legacy `auth <password>`. \
[x] 2026-07-02T05:21:10Z Implement and verify CMUX `settings open [target]` target alias parsing, bridge validation, activate parsing, and target echo output. \
[x] 2026-07-02T05:12:19Z Implement and verify CMUX `themes list/set/clear` Ghostty managed theme block parity with light/dark selections and JSON output. \
[x] 2026-07-02T05:06:48Z Implement and verify CMUX `settings open` live host settings dialog parity through the `settings.open` bridge method. \
[x] 2026-07-02T04:59:27Z Implement and verify CMUX workspace-group collapsed sidebar row hiding while keeping anchors and active workspaces visible. \
[x] 2026-07-02T04:55:46Z Implement and verify mode-specific rendered GTK right-sidebar panels for Files, Find, Vault, Sessions, Feed, and Dock previews. \
[x] 2026-07-02T04:51:03Z Implement and verify a rendered GTK right-sidebar metadata panel for CMUX mode/status/progress/log visibility. \
[x] 2026-07-02T04:43:27Z Implement and verify explicit CMUX Remote/SSH CLI `not_supported` surfacing while preserving no-socket help probes. \
[x] 2026-07-02T04:40:48Z Implement and verify CMUX sidebar metadata command help and non-JSON `sidebar-state` output parity. \
[x] 2026-07-02T04:37:18Z Implement and verify CMUX sidebar status/progress/log command state parity in CLI, live bridge, host state, and retained events. \
[x] 2026-07-02T04:25:15Z Implement and verify CMUX `right-sidebar` visibility, mode, focus, and state command parity in CLI and live bridge paths. \
[x] 2026-07-02T04:14:32Z Implement and verify CMUX command-position `--json` and `--id-format` presentation flag parity outside browser commands. \
[x] 2026-07-02T04:10:06Z Implement and verify no-socket CMUX command help probes for socket-backed commands and legacy aliases. \
[x] 2026-07-02T04:05:28Z Implement and verify CMUX `sessions list/debug` and `session-debug` no-socket session diagnostics with stale saved-PID reporting. \
[x] 2026-07-02T03:50:01Z Implement and verify CMUX tmux `list-windows`/`list-panes` format output parity plus `lsw`/`lsp` aliases. \
[x] 2026-07-02T03:45:39Z Implement and verify CMUX tmux short alias parity for `capturep`, `display/displayp`, `resizep`, `respawnp`, `setb`, `pasteb`, and `show-buffer/showb` with `-b` buffer names. \
[x] 2026-07-02T03:41:46Z Implement and verify CMUX tmux `display-message -p/-F/-t` format expansion parity with live Limux context, unknown-key stripping, trim, and fallback behavior. \
[x] 2026-07-02T02:00:00Z Implement and verify live bridge CMUX browser frame selection/main reset plus download wait CLI/API parity. \
[x] 2026-07-02T01:51:37Z Implement and verify CMUX workspace environment parity for `new-workspace --env/--env-file`, `workspace.create workspace_env`, terminal inheritance, and `workspace env --mask`. \
[x] 2026-07-02T01:39:20Z Implement and verify CMUX Feed `~/.cmuxterm/workstream.jsonl` audit persistence with bounded rotation. \
[x] 2026-07-02T01:35:42Z Implement and verify CMUX surface-targeted notification metadata parity for `created_at`, surface/pane refs, and `tab_title` rows/events. \
[x] 2026-07-02T01:30:11Z Implement and verify CMUX durable bounded `~/.cmuxterm/events.jsonl` event logging with 16 MiB rotation and oversized event-frame truncation. \
[x] 2026-07-02T01:27:16Z Implement and verify CMUX config `sidebar-font-size` and `surface-tab-bar-font-size` get/set parity while preserving unrelated Limux settings JSON. \
[x] 2026-07-02T01:05:00Z Refresh the CMUX audit clone to current `manaflow-ai/cmux` main commit `871aa710b30d0775f5f18f0bf98f57cdb42047f4` and verify the Limux fork is ahead of `am-will/main` without pushing to `am-will`. \
[x] 2026-07-02T01:05:00Z Implement and verify CMUX retained `events.stream` parity slice with replay ack metadata, category/name filters, heartbeat frames, and live socket takeover. \
[x] 2026-07-02T01:05:00Z Implement and verify CMUX Feed and agent event publication for `feed.item.received`, `feed.item.completed`, `feed.item.resolved`, and `agent.hook.<HookEventName>`. \
[x] 2026-07-02T01:05:00Z Implement and verify CMUX notification event publication for created, read, removed, and cleared notification lifecycle changes. \
[x] 2026-07-02T01:05:00Z Implement and verify `limux events --reconnect` resumes from the last processed sequence instead of failing before socket streaming. \
[x] 2026-07-02T00:58:43Z Implement and verify CMUX tmux `surface.respawn`/`respawn-pane`, stricter buffer/hook/display-message behavior, and explicit unsupported placeholder reporting. \
[x] 2026-07-02T00:44:15Z Implement and verify live bridge CMUX tmux `pane.swap` and `swap-pane` parity for exchanging selected surfaces between panes. \
[x] 2026-07-02T00:38:55Z Implement and verify live bridge CMUX tmux `pane.break` and `break-pane` parity for moving surfaces into new workspaces. \
[x] 2026-07-02T00:33:35Z Implement and verify live bridge CMUX tmux `pane.join` and `join-pane` parity for moving surfaces into target panes. \
[x] 2026-07-02T00:28:44Z Implement and verify live bridge CMUX tmux `pane.resize` and `resize-pane` parity by mutating persisted GTK split ratios. \
[x] 2026-07-02T00:22:28Z Implement and verify live bridge CMUX tmux `surface.clear_history` and `clear-history` parity for addressed or focused terminal surfaces. \
[x] 2026-07-02T00:16:33Z Implement and verify live bridge CMUX tmux `pane.last` and `last-pane` parity using live pane focus history. \
[x] 2026-07-02T00:11:57Z Implement and verify live bridge CMUX tmux workspace navigation parity for `next-window`, `previous-window`, and `last-window`. \
[x] 2026-07-02T00:05:19Z Implement and verify CLI and live bridge CMUX `surface.drag_to_split`, `split-off`, and `drag-surface-to-split` parity for moving surfaces into new split panes. \
[x] 2026-07-02T00:00:48Z Implement and verify CLI and live bridge CMUX `surface.split` and `new-split` parity for creating split terminal panes from surfaces. \
[x] 2026-07-01T23:57:14Z Implement and verify CLI and live bridge CMUX `surface.refresh` and `refresh-surfaces` parity for terminal surface redraws. \
[x] 2026-07-01T23:47:48Z Implement and verify CLI and live bridge CMUX `surface.reorder` and `reorder-surface` parity for reordering surfaces within panes. \
[x] 2026-07-01T23:41:59Z Implement and verify CLI and live bridge CMUX `surface.move` and `move-surface` parity for moving surfaces between panes. \
[x] 2026-07-01T23:38:20Z Implement and verify live bridge CMUX `surface.close` and `close-surface` parity for GTK surfaces. \
[x] 2026-07-01T23:33:49Z Implement and verify live bridge and CLI CMUX browser visible, enabled, and checked state checks. \
[x] 2026-07-02T03:53:00Z Implement and verify CMUX browser persisted element-ref parity for snapshot/find refs used by later actions and getters. \
[x] 2026-07-02T03:37:00Z Implement and verify retained CMUX browser console/error diagnostic buffers for live WebKit browser surfaces. \
[x] 2026-07-02T03:21:00Z Implement and verify live bridge CMUX browser dialog accept/dismiss CLI/API parity for WebKit JavaScript dialogs. \
[x] 2026-07-02T02:49:37Z Implement and verify CMUX surface I/O event emission for redacted text sends and key sends. \
[x] 2026-07-02T02:40:37Z Implement and verify CMUX workspace lifecycle event emission for create, select, close, rename, and reorder live-host mutations. \
[x] 2026-07-02T02:30:56Z Implement and verify CMUX notification hook policy effects for record, unread, desktop, and sound delivery decisions. \
[x] 2026-07-01T23:30:01Z Implement and verify live bridge CMUX browser state save and load parity for WebKit browser surfaces. \
[x] 2026-07-01T23:25:27Z Implement and verify live bridge CMUX browser tab list, new, switch, and close parity for WebKit browser surfaces. \
[x] 2026-07-01T23:14:21Z Implement and verify live bridge CMUX browser screenshot capture for WebKit browser surfaces. \
[x] 2026-07-01T22:59:57Z Implement and verify live bridge CMUX browser locator routing for WebKit browser surfaces. \
[x] 2026-07-01T22:54:12Z Implement and verify live bridge CMUX browser highlight, cookies, and Web Storage routing for WebKit browser surfaces. \
[x] 2026-07-01T22:47:41Z Implement and verify live bridge CMUX browser injection and console/error diagnostic routing for WebKit browser surfaces. \
[x] 2026-07-01T22:42:33Z Implement and verify explicit `not_supported` handling for CMUX-documented unsupported browser APIs in CLI and live bridge paths. \
[x] 2026-07-01T22:37:38Z Implement and verify Codex CMUX Feed hook setup wiring for installing and uninstalling `hooks feed --source codex` entries. \
[x] 2026-07-01T19:43:29Z Implement and verify the CMUX `hooks feed --source` CLI forwarding and decision-rendering parity slice for Feed permission, plan, and question events. \
[x] 2026-07-01T19:32:46Z Implement and verify the first CMUX Feed backend parity slice: `feed.push`, `feed.list`, `feed.permission.reply`, `feed.question.reply`, and `feed.exit_plan.reply`. \
[x] 2026-07-01T19:25:22Z Implement and verify the CMUX browser DOM action and selector-aware getter parity slice for live WebKit browser surfaces. \
[x] 2026-07-01T19:15:55Z Implement and verify the CMUX browser `snapshot` and bounded-polling `wait` parity slice for live WebKit browser surfaces. \
[x] 2026-07-01T19:06:02Z Implement and verify CMUX workspace-group mutation CLI/API parity for create, ungroup, delete, rename, collapse, expand, pin, unpin, add, remove, set-anchor, new-workspace, set-color, set-icon, move, and focus. \
[x] 2026-07-01T18:57:54Z Implement and verify the CMUX workspace-group read/persistence parity slice: persisted group metadata, workspace `group_id`, live `workspace.group.list`, `list-workspace-groups`, and `workspace-group list`. \
[x] 2026-07-01T18:13:19Z Convert the current CMUX and Limux inventories into a durable CMUX parity matrix with verification evidence and exact Limux files/tests for implemented or partial features. \
[x] 2026-07-01T18:13:19Z Build a Limux feature inventory from the current fork and map every CMUX feature to implemented, partial, missing, intentionally incompatible, or blocked. \
[x] 2026-07-01T18:13:19Z Build a complete current-CMUX feature inventory from commit 2313855c4988ea20e065a9a9e87413f014777f46, including CLI commands, server APIs, UI flows, workspace/session behavior, browser automation, notifications, agents, memory diagnostics, tmux/remote support, configuration, and persistence. \
[x] 2026-07-01T15:50:37Z Move completed TODO items to TODO_COMPLETED.md, commit verified changes, and push only to the fork remote. \
[x] 2026-07-01T15:50:37Z Verify with active-output benchmarks, targeted functionality checks, repository checks, and no pushes to am-will. \
[x] 2026-07-01T15:50:37Z Implement or patch missing CMUX parity functionality selected from the audit. \
[x] 2026-07-01T15:50:37Z Implement the highest-confidence additional performance or leak fixes found in the audit. \
[x] 2026-07-01T15:50:37Z Identify missing high-value CMUX functionality or behavioral gaps that can be migrated without breaking existing Limux workflows. \
[x] 2026-07-01T15:50:37Z Pull or inspect the current CMUX codebase and build a concrete CMUX-to-Limux functionality parity matrix. \
[x] 2026-07-01T15:50:37Z Re-audit Limux runtime code paths for additional active-usage CPU, memory, GPU, disk-write, latency, and leak bottlenecks. \
[x] 2026-07-01T13:58:15Z Move completed active-output TODO entries to TODO_COMPLETED.md, commit verified changes, and push only to the fork remote. \
[x] 2026-07-01T13:58:15Z Re-run active-output profiling and repository checks to verify runtime efficiency, stability, and no continuous disk writes. \
[x] 2026-07-01T13:58:15Z Patch the verified active-runtime bottleneck while preserving terminal functionality. \
[x] 2026-07-01T13:58:15Z Capture active-output baseline metrics for the triple workload. \
[x] 2026-07-01T13:58:15Z Extend the mixed workload profiler to measure active-output CPU, RSS, disk writes, GPU state, and stability separately from creation and idle metrics. \
[x] 2026-07-01T13:58:15Z Define an active-output workload with many open terminals repeatedly printing text while Limux stays running. \
[x] 2026-07-01T13:34:55Z Move completed triple-workload TODO entries to TODO_COMPLETED.md, commit the verified changes, and push only to the fork remote. \
[x] 2026-07-01T13:34:55Z Run repository verification after any code/script/docs changes. \
[x] 2026-07-01T13:34:55Z Re-run the optimized triple workload and verify whether the 80% creation-latency target is achieved or blocked by a measured constraint. \
[x] 2026-07-01T13:34:55Z Patch the dominant triple-workload bottleneck while preserving existing Limux behavior and hard-failing unsafe split batching. \
[x] 2026-07-01T13:34:55Z Measure the triple sequential baseline for latency, CPU, RSS, disk writes, GPU state, pane count, surface count, and stability. \
[x] 2026-07-01T13:34:55Z Define the triple mixed workload as 12 workspaces, 48 split panes, and 120 terminal surfaces. \
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
[x] 2026-07-02T02:56:15Z Implement and verify CMUX `events.stream` slow-consumer cutoff at 1,024 pending live events with an explicit `slow_consumer` error frame. \
[x] 2026-07-02T03:02:31Z Implement and verify retained CMUX surface lifecycle events for successful create, focus, close, move, reorder, and drag-to-split mutations. \
[x] 2026-07-02T03:11:16Z Implement and verify live CMUX `config reload`/`reload-config` parity with settings and shortcut reload, terminal appearance/font application, and retained `config.reloaded` event emission. \
[x] 2026-07-02T03:24:41Z Implement and verify retained CMUX browser navigation, interaction, and redacted input event emission for successful live browser actions. \
[x] 2026-07-02T03:38:12Z Implement and verify CMUX `read-screen`/`capture-pane` `--lines` and `--scrollback` request plumbing, last-N visible text limiting, and explicit scrollback capability metadata. \
[x] 2026-07-02T03:48:03Z Implement and verify CMUX `workspace reconnect`/`workspace disconnect` CLI/API routing with explicit `not_supported` errors until remote daemon support exists. \
