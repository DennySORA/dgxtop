<!-- ops-tools:workflow:v1:start -->
# Engineering handoff

Check source snapshots, unknown outcomes, and writer state first; records do not grant new execution authorization.

- TKT-000001: Blocked; The functionality and local validation are complete; run target/release/dgxtop to use it. Only the workflow runner receipt remains to be recorded when the environment supports it.
  Checkpoint 2026-10-03T17:17:38Z: Goal: scroll long GPU process lists, show the GPU column first, and filter by GPU. Source snapshot 6d7a8580b759cdeea1ad931f286b05ed883ffacfd4352407675d8520ea0ca681; product implementation, 7 regression tests, 28 regular tests, 3 GPU tests, release PTY validation, and diff review are complete. Evidence EV-000001 is agent_reported. No agent writes or operations with unknown outcomes are in progress; existing adjacent collector issues are tracked separately. No commits, installations, or releases were performed. The workflow ticket is blocked only because the isolated runner is unavailable.
<!-- ops-tools:workflow:v1:end -->
