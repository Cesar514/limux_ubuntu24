// Current goal: keep this TODO list accurate while implementing the user's current request. \
// Rule: complete all tasks asked by the user and verify they are fully completed before moving them to TODO_COMPLETED.md. \
// Format: [] <priority>. <task> where <priority> is plain decimal digits followed immediately by a period, for example [] 1. First task, [] 2. Second task, [] 14. Final task. Priority numbers must be strictly increasing and never repeated. Do not use 1), (1), 1:, -, *, or any numbering format other than <digits>. \

[] 1. Fetch and integrate the latest upstream limux changes without discarding local fork work. \
[] 2. Baseline the project build/test state after updating from upstream. \
[] 3. Audit high-risk leak, socket, filesystem, process, and terminal-control paths that could expose data or damage user files. \
[] 4. Patch validated safety issues without adding silent fallbacks. \
[] 5. Evaluate whether a language conversion is useful now and keep existing functionality intact. \
[] 6. Compare Limux with similar terminal multiplexers and document practical missing functions. \
[] 7. Verify the patched behavior with the strongest local checks available. \
[] 8. Move completed work items to TODO_COMPLETED.md and leave TODO.md clean. \
