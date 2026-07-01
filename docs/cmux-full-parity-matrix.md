# CMUX Full Parity Matrix

Source CMUX commit: `2313855c4988ea20e065a9a9e87413f014777f46` in `/tmp/cmux-audit`.

Status values:

- Implemented: Limux has a verified equivalent in this branch.
- Partial: Limux has a usable subset, but named CMUX behavior remains missing.
- Missing: no equivalent found in Limux.
- Blocked: implementation needs external infrastructure or a platform facility not available in this local verification pass.
- Not supported by CMUX: upstream CMUX documents the API as unsupported.

| Area | CMUX commands/APIs | Limux status | Evidence and remaining gap |
| --- | --- | --- | --- |
| Notification lifecycle | `notify`, `list-notifications`, `dismiss-notification`, `mark-notification-read`, `open-notification`, `jump-to-unread`, `clear-notifications`; `notification.create/list/dismiss/mark_read/open/jump_to_unread/clear` | Partial | Implemented live host store and CLI/API aliases in `rust/limux-host-linux/src/control_bridge.rs`, `rust/limux-host-linux/src/window.rs`, and `rust/limux-cli/src/main.rs`. Verified by `cargo test -p limux-host-linux notification -- --nocapture` and `cargo test -p limux-cli cmux_ -- --nocapture`. Remaining: CMUX notification events, hook policy effects, surface-scoped notification rows, `created_at`, and `tab_title`. |
| Browser open split | `browser open`, `browser open-split`, `browser new`; `browser.open_split` | Partial | Live bridge now accepts `pane.create type=browser` and `browser.open_split`, seeded through WebKit browser pane state. Remaining: broader browser automation backend. |
| Browser navigation and automation | `goto/navigate`, `back/forward/reload`, `snapshot`, `eval`, `wait`, `click`, `fill`, `type`, `select`, `get`, `find`, `frame`, `dialog`, `download`, `cookies`, `storage`, `tab`, `console`, `errors`, `highlight`, `state` | Partial | Live bridge now supports `browser.navigate`, `browser.url.get`, `browser.back`, `browser.forward`, `browser.reload`, `browser.focus_webview`, `browser.is_webview_focused`, `browser.eval`, and read-only getters `browser.get.title/text/html` for addressed WebKit browser surfaces. CLI routes matching browser namespace and legacy focus/url aliases in `rust/limux-cli/src/main.rs`. Missing live socket backend for snapshots, wait, mutating DOM interactions, element locators, cookies/storage, tabs, diagnostics, injection, and state APIs. |
| Browser documented unsupported APIs | `browser.viewport.set`, `browser.geolocation.set`, `browser.offline.set`, `browser.trace.start/stop`, `browser.network.route/unroute/requests`, `browser.screencast.start/stop`, `browser.input_mouse/input_keyboard/input_touch` | Not supported by CMUX | Upstream documents these as WKWebView gaps in `/tmp/cmux-audit/skills/cmux-browser/references/commands.md`. Limux should return explicit unsupported errors if these are routed. |
| Workspace lifecycle | `workspace list/create/env/close/rename/select/reconnect/disconnect`, `list-workspaces`, `new-workspace`, `select-workspace`, `close-workspace`, `rename-workspace`, `current-workspace` | Partial | Core workspace list/create/select/rename/close exists. Remaining: reconnect/disconnect, env, moving/reordering across windows, and full CMUX argument compatibility. |
| Workspace groups | `workspace-group list/create/ungroup/delete/rename/collapse/expand/pin/unpin/add/remove/set-anchor/new-workspace/set-color/set-icon/move/focus`; `workspace.group.*` | Partial | Added CMUX-style persisted group model, per-workspace `group_id`, live `workspace.group.list`/`list-workspace-groups`, and CLI `workspace-group list` read path. Verified by `cargo test -p limux-host-linux -- --nocapture` and `cargo test -p limux-cli -- --nocapture`. Remaining: create/delete/rename/collapse/expand/pin/unpin/add/remove/set-anchor/new-workspace/set-color/set-icon/move/focus mutations plus grouped sidebar UI behavior. |
| Pane and surface lifecycle | `new-split`, `list-panes`, `list-pane-surfaces`, `focus-pane`, `new-pane`, `new-surface`, `close-surface`, `move-surface`, `split-off`, `reorder-surface`, `drag-surface-to-split`, `refresh-surfaces`, `surface-health` | Partial | List/create/focus and terminal surface creation are partly implemented. Remaining: close/move/reorder/split-off/drag/refresh live GTK mutations and full pane resize/swap/break/join parity. |
| Terminal I/O | `read-screen`, `capture-pane`, `send`, `send-key`, `send-panel`, `send-key-panel` | Partial | Terminal read/send/key paths exist. Remaining: exact CMUX scrollback/options parity and event emission. |
| Events stream | `events.stream`; CLI `events --after --cursor-file --name --category --reconnect --limit --no-ack --no-heartbeat` | Partial | Added CMUX-shaped `events.stream` ack takeover on the live socket and CLI `events` flag parsing/JSONL reader path. Remaining: real retained replay buffer, live event publication, heartbeats, reconnect loop, cursor updates from event frames, durable bounded JSONL log, and event emission from workspace/pane/surface/browser/notification/feed/agent actions. |
| Feed and approvals | `feed.push`, `feed.permission.reply`, `feed.question.reply`, `feed.exit_plan.reply`; feed TUI/hooks | Missing | Limux has agent hooks and `agent-team`, but no CMUX feed/approval protocol or blocking approval UX. |
| Remote and SSH | `ssh`, `remote list/add/remove`, `remote-daemon-status`, `ssh-session-*`; `workspace.remote.*`; `cmuxd-remote` daemon RPC | Missing | No CMUX-compatible remote daemon, SSH workspace proxy, lease/auth file protocol, or remote PTY/session RPC found. |
| tmux compatibility | `capture-pane`, `resize-pane`, `pipe-pane`, `wait-for`, `swap-pane`, `break-pane`, `join-pane`, `next-window`, `previous-window`, `last-window`, `last-pane`, `find-window`, `clear-history`, buffers/hooks/respawn/display-message | Partial | Several compatibility stubs and aliases exist in CLI. Missing exact tmux semantics for pane/window movement, wait/buffers/hooks, and unsupported placeholder reporting. |
| tmux documented unsupported placeholders | `popup`, `bind-key`, `unbind-key`, `copy-mode` | Not supported by CMUX | Upstream CLI contract marks these as placeholders/unsupported. Limux should report explicit unsupported behavior if exposed. |
| Config/settings/themes/docs | `settings`, `config doctor/check/validate/path/paths/docs/reload/get/set`, `themes list/set/clear`, `docs settings/shortcuts/api/browser/agents` | Partial | Implemented no-socket CLI parity for `docs`, `settings path/docs`, `shortcuts`, `config path/paths/check/validate/doctor/docs/documentation`, `themes list/set/clear`, and explicit unsupported errors for host-only `settings open` and `config reload`. Remaining: live host settings UI/reload API, config precedence beyond XDG Limux settings/shortcuts, broader CMUX theme catalog names, and any upstream `config get/set` behavior confirmed outside `docs/cli-contract.md`. |
| CLI globals and environment | `--socket`, `--password`, `--json`, `--id-format`, `--window`; `CMUX_SOCKET_PATH`, `CMUX_SOCKET`, `CMUX_SOCKET_PASSWORD`, `CMUX_WORKSPACE_ID`, `CMUX_SURFACE_ID`, `CMUX_TAB_ID` | Partial | Limux now supports `CMUX_SOCKET_PATH`/`CMUX_SOCKET` checked routing, global `--window` parsing for window aliases, global `--password` parsing, no-socket `help`/`version`, and CMUX workspace/surface/tab env aliases in CLI context lookup and spawned terminal env. Remaining: password-auth semantics, full multi-window routing, command-position presentation flags outside browser commands, and broader CMUX command-help probes. |

Primary upstream references:

- `/tmp/cmux-audit/docs/cli-contract.md`
- `/tmp/cmux-audit/docs/events.md`
- `/tmp/cmux-audit/docs/feed.md`
- `/tmp/cmux-audit/docs/notifications.md`
- `/tmp/cmux-audit/docs/workspace-groups.md`
- `/tmp/cmux-audit/docs/remote-daemon-spec.md`
- `/tmp/cmux-audit/daemon/remote/README.md`
- `/tmp/cmux-audit/skills/cmux-browser/references/commands.md`
