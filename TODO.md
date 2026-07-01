// Current goal: keep this TODO list accurate while implementing the user's current request. \
// Rule: complete all tasks asked by the user and verify they are fully completed before moving them to TODO_COMPLETED.md. \
// Format: [] <priority>. <task> where <priority> is plain decimal digits followed immediately by a period, for example [] 1. First task, [] 2. Second task, [] 14. Final task. Priority numbers must be strictly increasing and never repeated. Do not use 1), (1), 1:, -, *, or any numbering format other than <digits>. \

[] 4. Implement missing CMUX CLI/API parity features in priority order without removing existing Limux functionality. \
[] 5. Implement missing CMUX UI/workspace/session parity features in priority order without adding silent fallbacks. \
[] 6. Implement missing CMUX browser/automation/notification/agent parity features in priority order without unsafe credentials or hidden defaults. \
[] 7. Implement missing CMUX tmux/remote/config/persistence parity features in priority order or document concrete blockers where direct local verification is impossible. \
[] 8. Add or update tests and runtime checks that directly prove each migrated CMUX feature works in Limux. \
[] 9. Run active multi-workspace and multi-terminal performance/leak/disk-write/GPU verification after parity patches and fix regressions found there. \
[] 10. Move completed TODO items to TODO_COMPLETED.md, commit verified parity changes, and push only to the fork remote. \
