// summary: Track the safety, modernization, and feature-parity goal requested for Limux.
// purpose: Give a self-contained execution plan for updating upstream, auditing risky paths, patching issues, and validating behavior.
// inputs: Local Limux checkout, upstream git remotes, Rust build/test commands, and terminal multiplexer feature comparisons.
// returns/effects: Documents implementation decisions, verification steps, and completion evidence for the current goal.

# Safety Modernization and Feature-Parity ExecPlan

## Purpose / Big Picture

This plan updates the local Limux checkout with the latest `am-will/limux` main branch, then improves the project where concrete safety or reliability risks are found. Limux is already implemented primarily in Rust around a Ghostty integration, so this plan treats "convert to a more efficient programming language" as an engineering decision to evaluate rather than an automatic rewrite. The user-visible outcome is a safer terminal application that preserves existing functionality, has verified tests, and has a documented feature-parity path against similar terminal multiplexers.

## Progress

- [x] (2026-07-01 10:49Z) Created the goal and added current tasks to TODO.md.
- [x] (2026-07-01 10:55Z) Fetched `am-will/limux` and started merging `am-will/main` into local `main`.
- [x] (2026-07-01 10:58Z) Resolved the only source merge conflict in `rust/limux-host-linux/src/layout_state.rs` by preserving both the local file header and upstream imports.
- [x] (2026-07-01 10:45Z) Completed the upstream merge commit `caa5ac4` and verified `cargo fmt --all --check`, `cargo test --workspace`, and `cargo clippy --workspace --all-targets -- -D warnings` on the merged baseline.
- [x] (2026-07-01 11:14Z) Audited high-risk local runtime paths for leakage, data loss, filesystem damage, socket/auth, terminal escape, process, and FFI problems with a parent-agent pass and a read-only explorer subagent.
- [x] (2026-07-01 11:31Z) Patched validated safety issues in socket authorization, FFI request bounds, CLI wait markers, session/config loading, and shortcut loading.
- [x] (2026-07-01 11:36Z) Compared Limux against similar terminal multiplexers and documented implementable missing functions.
- [ ] Run final verification and close TODO.md.

## Surprises & Discoveries

- Observation: The local repository origin is `https://github.com/Cesar514/limux_ubuntu24.git`, while the requested fork/upstream URL is `https://github.com/am-will/limux`.
  Evidence: `git remote -v` showed the Cesar514 origin; `git ls-remote --symref https://github.com/am-will/limux.git HEAD` showed `main` at `9ffc9341de2ad649f99a85df7c05b7eafb4a6236`.
- Observation: Fetching `am-will/limux` attempted to fetch the Ghostty submodule and reported a missing old ref, but both local `HEAD` and `am-will/main` point `ghostty` at `81ab8ffa90185221782baf785e85387321e16f8d`.
  Evidence: `git ls-tree HEAD ghostty` and `git ls-tree am-will/main ghostty` both returned the same submodule commit.
- Observation: Limux is already mostly Rust, so a full language conversion would add risk without satisfying a clear missing capability.
  Evidence: The repository root has `Cargo.toml`, Rust crates under `rust/`, and Rust sources for CLI, control socket, host UI, and protocol layers.
- Observation: Malformed canonical session, app settings, and canonical shortcut files previously produced default-shaped state rather than aborting load.
  Evidence: `rust/limux-host-linux/src/layout_state.rs`, `rust/limux-host-linux/src/app_config.rs`, and `rust/limux-host-linux/src/shortcut_config.rs` tests were updated from fallback expectations to explicit rejection expectations.
- Observation: The CLI `wait-for` compatibility command used predictable marker paths under `/tmp`.
  Evidence: `rust/limux-cli/src/main.rs` now scopes markers under `env::temp_dir()/limux-cli/<socket-hash>/wait/` and creates them with `create_new(true)`.

## Decision Log

- Decision: Merge `am-will/main` into local `main` instead of resetting or rebasing.
  Rationale: The local branch is ahead by one commit, and user instructions forbid discarding local work. A merge preserves both histories.
  Date/Author: 2026-07-01 / Codex.
- Decision: Treat a full Rust conversion as already satisfied by architecture evaluation unless the audit finds a non-Rust component that should be replaced.
  Rationale: Rewriting working Rust code or vendored Ghostty Zig/C APIs would be higher risk than targeted safety patches.
  Date/Author: 2026-07-01 / Codex.
- Decision: Keep competitor feature additions as a documented roadmap rather than implementing them in the same patch set.
  Rationale: Browser automation parity, notification history, broadcast groups, pane resizing, sidebar metadata, and profiles each touch broad UI/control surfaces. Mixing those changes with security/data-loss fixes would make verification weaker.
  Date/Author: 2026-07-01 / Codex.
- Decision: Default the control socket policy to descendant-only and reject unknown socket mode values.
  Rationale: The socket can inject text/keys and create command-running panes; accepting all same-UID processes by default is too broad for the current hard-cutout goal.
  Date/Author: 2026-07-01 / Codex.

## Outcomes & Retrospective

This section will be completed after final verification. It must state what was patched, what remained intentionally unimplemented, and which verification commands prove the goal.

## Context and Orientation

The repository is a Rust workspace for Limux, a Linux terminal application that integrates Ghostty. The key local Rust crates are `rust/limux-host-linux` for the GTK host UI and terminal panes, `rust/limux-control` for control-socket behavior, `rust/limux-protocol` for protocol types, `rust/limux-cli` for command-line entry points, and `rust/limux-ghostty-sys` for Ghostty FFI bindings. A git submodule or vendored dependency named `ghostty` contains upstream terminal engine code and should not be rewritten as part of this goal unless a direct integration bug requires it.

The strongest likely risk surfaces are session persistence in `rust/limux-host-linux/src/layout_state.rs`, control socket path and authentication code in `rust/limux-control/src/`, subprocess spawning and terminal working-directory handling in `rust/limux-host-linux/src/terminal.rs` and related host modules, CLI compatibility state in `rust/limux-cli/src/main.rs`, and unsafe FFI in `rust/limux-control/src/ffi.rs` plus `rust/limux-ghostty-sys`.

## Feature-Parity Findings

Limux already has panes, workspaces, tabs, live control commands, notifications, browser tabs, and agent team launching. The highest-value missing functions compared with similar tools are:

1. Browser command bridge parity. Limux documents and implements browser tabs, but the live GTK control bridge still rejects or omits broader browser automation. This maps to `rust/limux-host-linux/src/control_bridge.rs`, `rust/limux-host-linux/src/window.rs`, `rust/limux-host-linux/src/pane.rs`, and `rust/limux-cli/src/main.rs`.
2. Notification inbox and jump-to-unread. Limux can create notifications and mark sidebar unread state, but it does not yet expose a durable notification list or jump command. This maps to `window.rs`, `layout_state.rs`, and CLI/control methods.
3. Broadcast input to explicit pane groups. tmux exposes synchronized panes and Terminator documents grouped/broadcast input. Limux currently sends text/key input to one target surface. This should be explicit-only, for example `--all-terminals`, because broadcast input can be destructive.
4. Keyboard and CLI pane resize. tmux documents `resize-pane`, and Terminator exposes zoom/maximize and layout manipulation. Limux persists split ratios but lacks an explicit `pane.resize` control method.
5. Rich workspace sidebar metadata. cmux-like workflows benefit from cwd, git branch, recent notification, and port/status metadata in sidebars. Limux already stores workspace folder/favorite/unread state, so this fits a small metadata module.
6. Terminal profiles and per-pane appearance. Terminator supports profiles and profile switching. Limux currently has global app appearance/font settings, so a scoped profile model could preserve existing defaults while adding per-tab assignment.

External references checked on 2026-07-01: tmux manual page for pane resize and pane/window capabilities at `https://man7.org/linux/man-pages/man1/tmux.1.html`; Terminator grouping documentation at `https://terminator-gtk3.readthedocs.io/en/latest/grouping.html`; Terminator config/profile documentation at `https://manpages.ubuntu.com/manpages/bionic/man5/terminator_config.5.html`; cmux README at `https://github.com/manaflow-ai/cmux`; and cmux Linux browser automation notes at `https://github.com/bradwilson331/cmux-linux`. The plan does not depend on exact competitor implementation internals.

## Plan of Work

First, finish the merge from `am-will/main` and verify the working tree has no unresolved conflicts. Then run the repository's existing Rust checks to establish a baseline. Next, inspect the high-risk runtime surfaces and patch only concrete problems, favoring explicit fatal errors over silent fallbacks. After safety patches, compare Limux's current feature set with established terminal multiplexers and record missing practical functions in documentation unless a small high-confidence feature can be implemented safely in the current pass. Finally, run formatting, tests, and any available smoke checks, then move all current TODO.md tasks to TODO_COMPLETED.md.

## Concrete Steps

Run all commands from `/home/cesar514/Documents/agent_programming/limux_ubuntu24`.

Fetch and merge upstream:

    git remote add am-will https://github.com/am-will/limux.git 2>/dev/null || git remote set-url am-will https://github.com/am-will/limux.git
    git fetch --tags am-will
    git merge --no-edit am-will/main

If a conflict appears in `rust/limux-host-linux/src/layout_state.rs`, keep the structured file header and upstream collection imports, then run:

    git add rust/limux-host-linux/src/layout_state.rs
    git commit

Baseline and final verification should use:

    cargo fmt --all --check
    cargo test --workspace
    cargo clippy --workspace --all-targets -- -D warnings

If a repository script wraps these checks more precisely, run that script as well:

    scripts/check.sh

## Validation and Acceptance

Acceptance requires that the local branch contains the latest `am-will/main` changes plus the local fork commit, no merge conflicts remain, high-risk validated issues are patched, and the Rust workspace checks pass or have a concrete environment blocker. The safety audit is accepted only when each promoted issue has either a code patch and test/verification evidence or a documented reason for not changing code. The feature-parity review is accepted when it cites Limux evidence and similar-tool capabilities with scoped MVP acceptance criteria.

## Idempotence and Recovery

The fetch step is safe to repeat. The merge step should not be reset destructively; if conflicts recur, inspect with `git status` and resolve specific files by hand. The safety patches should be small and reviewable. If tests fail due to missing system libraries, record the exact missing package or linker error and run the strongest remaining checks.

## Artifacts and Notes

The only merge conflict observed so far was a header/import conflict in `rust/limux-host-linux/src/layout_state.rs`. The resolved top of file should begin with the structured four-line header, followed by:

    use std::collections::hash_map::Entry;
    use std::collections::BTreeMap;
    use std::collections::HashMap;

## Interfaces and Dependencies

No new runtime dependencies should be added unless a concrete safety patch requires them. Existing Rust crates and standard library APIs should be preferred. Session persistence must remain JSON-compatible with existing `session.json` and legacy `workspaces.json` data unless a deliberate migration is implemented and tested.
