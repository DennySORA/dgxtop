<!-- ops-tools:workflow:v1:start -->
# TKT-000001 GPU process list scrolling, GPU identification, and filtering

Status: Blocked · revision 13

Next action: The functionality and local validation are complete; run target/release/dgxtop to use it. Only the workflow runner receipt remains to be recorded when the environment supports it.

## Acceptance

- [ ] TAC-001 Overview and Processes can scroll to the last row; the GPU column comes first; GPU and text filters compose; selection survives refresh; local Rust checks and diff review pass · Blocked

## Open issues

- ISS-001: The workflow evidence runner cannot establish verified OS isolation; next action: When the environment supports the existing isolated runner, record the same local checks again, then run verify
- ISS-002: The existing collector recalculates CPU deltas for the same PID on multiple GPUs, and its compute/graphics memory merge differs from the GPU collector; next action: Handle separately: design collector tests for one sample per PID and consistent memory merging

## Recent worklog

- 2026-10-03T16:59:41Z [progress] Preserve the source; observation: The source awaits analysis; no attachments were executed; next action: Confirm coverage and update the ticket
- 2026-10-03T17:01:26Z [transition] Update ticket status; observation: Confirmed the source, scope, and acceptance criteria; next action: Share a stateful table; implement filtering and long-list tests
- 2026-10-03T17:01:26Z [transition] Update ticket status; observation: Started the fix requested by the user; next action: Share a stateful table; implement filtering and long-list tests
- 2026-10-03T17:09:04Z [progress] Completed the shared process table, GPU filtering, keyboard scrolling, and 6 regression tests; observation: Two read-only workers checked the UI and data sources, confirming (gpu_index, pid) identity, clipping from stateless tables, and insufficient Overview height. Adopted their recommendations for filtering, selection, and K-key guards. Review found that the first load incorrectly preserved an index from the new data; fixed it and added a test. cargo test --offline --locked --test process_ui: 6 passed. The earlier 4 passed/1 failed result was caused by the test text extractor not skipping continuation cells for wide CJK characters; fixed the extractor. Quality inspection exposed existing MSRV/lint strict-policy gaps; no new rules were adopted and dependencies were not changed. Initial worklog writes were blocked by group-writable directory permissions. Temporarily tightened permissions on the project root and docs to recover successfully; restore them when finished.; next action: Run the complete local Rust quality checks and a native terminal smoke test, then review the final diff.
- 2026-10-03T17:09:39Z [transition] Update ticket status; observation: Added the other language READMEs to documentation scope; updated the local writer contract before continuing; next action: Run the complete local Rust quality checks and a native terminal smoke test, then review the final diff.
- 2026-10-03T17:10:34Z [transition] Update ticket status; observation: Keep Rust acceptance within the original owner scope; review the other language documentation diffs separately; next action: Run the complete local Rust quality checks and a native terminal smoke test, then review the final diff.
- 2026-10-03T17:17:38Z [check_result] Completed the final local Rust gates, hardware GPU/PTY checks, and integrated diff review; observation: Final regular tests: 28 passed, including 7 new UI regressions; GPU tests: 3 passed. cargo build/check/fmt/clippy/test/release all exited 0. Native release PTY validation at 120x24 succeeded; the program exited 0. The second read-only review found no blocking data issues. Adopted the recommendation to hide inactive shortcuts in Detail. Hardware validation found blank space after enlarging the viewport; fixed the offset limit and tested it.; next action: The functionality and local validation are complete; run target/release/dgxtop to use it. Only the workflow runner receipt remains to be recorded when the environment supports it.
- 2026-10-03T17:17:38Z [transition] Update ticket status; observation: Product work is complete; machine-recorded acceptance is blocked by runner_isolation_unavailable. Preserve the direct check evidence.; next action: The functionality and local validation are complete; run target/release/dgxtop to use it. Only the workflow runner receipt remains to be recorded when the environment supports it.
- 2026-10-03T17:17:38Z [checkpoint] Save a resumable checkpoint; observation: Goal: scroll long GPU process lists, show the GPU column first, and filter by GPU. Source snapshot 6d7a8580b759cdeea1ad931f286b05ed883ffacfd4352407675d8520ea0ca681; product implementation, 7 regression tests, 28 regular tests, 3 GPU tests, release PTY validation, and diff review are complete. Evidence EV-000001 is agent_reported. No agent writes or operations with unknown outcomes are in progress; existing adjacent collector issues are tracked separately. No commits, installations, or releases were performed. The workflow ticket is blocked only because the isolated runner is unavailable.; next action: The functionality and local validation are complete; run target/release/dgxtop to use it. Only the workflow runner receipt remains to be recorded when the environment supports it.

See ticket.json for the complete history and acceptance status.
<!-- ops-tools:workflow:v1:end -->
