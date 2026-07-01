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
- [ ] Complete the upstream merge commit and baseline verification.
- [ ] Audit high-risk local runtime paths for leakage, data loss, filesystem damage, socket/auth, terminal escape, process, and FFI problems.
- [ ] Patch validated safety issues.
- [ ] Compare Limux against similar terminal multiplexers and document implementable missing functions.
- [ ] Run final verification and close TODO.md.

## Surprises & Discoveries

- Observation: The local repository origin is `https://github.com/Cesar514/limux_ubuntu24.git`, while the requested fork/upstream URL is `https://github.com/am-will/limux`.
  Evidence: `git remote -v` showed the Cesar514 origin; `git ls-remote --symref https://github.com/am-will/limux.git HEAD` showed `main` at `9ffc9341de2ad649f99a85df7c05b7eafb4a6236`.
- Observation: Fetching `am-will/limux` attempted to fetch the Ghostty submodule and reported a missing old ref, but both local `HEAD` and `am-will/main` point `ghostty` at `81ab8ffa90185221782baf785e85387321e16f8d`.
  Evidence: `git ls-tree HEAD ghostty` and `git ls-tree am-will/main ghostty` both returned the same submodule commit.
- Observation: Limux is already mostly Rust, so a full language conversion would add risk without satisfying a clear missing capability.
  Evidence: The repository root has `Cargo.toml`, Rust crates under `rust/`, and Rust sources for CLI, control socket, host UI, and protocol layers.

## Decision Log

- Decision: Merge `am-will/main` into local `main` instead of resetting or rebasing.
  Rationale: The local branch is ahead by one commit, and user instructions forbid discarding local work. A merge preserves both histories.
  Date/Author: 2026-07-01 / Codex.
- Decision: Treat a full Rust conversion as already satisfied by architecture evaluation unless the audit finds a non-Rust component that should be replaced.
  Rationale: Rewriting working Rust code or vendored Ghostty Zig/C APIs would be higher risk than targeted safety patches.
  Date/Author: 2026-07-01 / Codex.

## Outcomes & Retrospective

This section will be completed after final verification. It must state what was patched, what remained intentionally unimplemented, and which verification commands prove the goal.

## Context and Orientation

The repository is a Rust workspace for Limux, a Linux terminal application that integrates Ghostty. The key local Rust crates are `rust/limux-host-linux` for the GTK host UI and terminal panes, `rust/limux-control` for control-socket behavior, `rust/limux-protocol` for protocol types, `rust/limux-cli` for command-line entry points, and `rust/limux-ghostty-sys` for Ghostty FFI bindings. A git submodule or vendored dependency named `ghostty` contains upstream terminal engine code and should not be rewritten as part of this goal unless a direct integration bug requires it.

The strongest likely risk surfaces are session persistence in `rust/limux-host-linux/src/layout_state.rs`, control socket path and authentication code in `rust/limux-control/src/`, subprocess spawning and terminal working-directory handling in `rust/limux-host-linux/src/terminal.rs` and related host modules, and unsafe FFI in `rust/limux-ghostty-sys`.

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
