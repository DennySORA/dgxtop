# dgxtop v2 rewrite — work record

Hand-written record (not machine-managed: the `tools project-workflow` engine is not available
on this machine). Ticket status below is the single status list for this rewrite; evidence is
in `docs/validation/`.

## Scope

Complete rewrite of dgxtop according to `spec/dgxtop-v2/` (40 requirements R01–R40, 15 work
packages T01–T15, 40 acceptance cases AT01–AT40): six components (Adapter, Collector, Manager,
Store, Viewer, Controller), source arbitration, bounded history, versioned configuration,
protected pidfd process actions, ten display modules, packaging and rollback. The previous
implementation was removed; a reference copy was kept outside the repository during the work.

Non-goals (spec §1): remote services/HTTP, GPU reset/fan/power control, dynamic plugins,
external TSDB, unverified vendors.

Carried-over user requirement from TKT-000001 (legacy, superseded by this rewrite): long process
lists must scroll to the last row, the GPU column comes first, and the process table can be
filtered to one GPU while the selection survives refresh. It is part of T10 acceptance.

## Tickets

| Ticket | Requirements / acceptance | Owner path | Status |
|---|---|---|---|
| T01 baseline, rules, toolchain | R19 / AT19 | `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml` | done |
| T02 core contracts and byte budget | R02, R21 / AT02, AT21 | `crates/core` | done |
| T03 runtime, supervisor, IPC | R14, R22, R24, R33 / AT14, AT22, AT24, AT33 | `crates/runtime` | done |
| T04 read-only adapters | R01, R15, R34 / AT01, AT15, AT34 | `crates/adapters` | done (real-host smoke on an x86_64 workstation; DGX Spark blocked) |
| T05 collectors and arbitration | R03, R04, R09 / AT03, AT04, AT09 | `crates/collectors` | done |
| T06 hot store and exact history | R07, R27 / AT07, AT27 | `crates/store` (hot, history, query) | done |
| T07 config and persistent stores | R06, R08, R31, R36 / AT06, AT08, AT31, AT36 | `crates/store` (config, audit, archive) | done |
| T08 manager use cases and sessions | R05, R23 / AT05, AT23 | `crates/manager` | done |
| T09 protected process action | R10, R32 / AT10, AT32 | `crates/adapters/process_action`, `crates/manager`, `tests/action_disposable` | done |
| T10 viewer, controller, all views | R11, R12, R13, R16, R25, R26 / AT11–AT13, AT16, AT25, AT26 | `crates/ui`, `crates/manager/presentation`, `tests/pty` | done; overview redesigned per owner (D-36), Sources/Processes tabs hidden |
| T11 observability and fault injection | R17, R30, R38, R39 / AT17, AT30, AT38, AT39 | all crates, `tests/fault_injection` | partial: metrics output (AT17 S2) not implemented |
| T12 performance profiling and REF | R28, R35 / AT28, AT35 | `benches/`, REF generator | harness and smoke runs done; the 3 x 30 min acceptance runs were not run |
| T13 72 h soak and hardware matrix | R29 / AT29 | target hardware | blocked (72 h runs and DGX Spark hardware not available in this session) |
| T14 packaging, CI, migration, rollback | R18, R20, R37 / AT18, AT20, AT37 | `.github/workflows`, `install.sh`, `scripts/`, `tests/cli` | done locally (installer tested against a local release); no tagged release was built |
| T15 per-requirement validation | R40 / AT40 | `docs/validation/`, `scripts/check_validation.py` | done: every AT has a checked status in `docs/validation/VALIDATION.md` (browser HTML checks of the spec not run) |

## Execution notes

- Parallel implementation: crates were implemented by parallel workers in isolated snapshots
  against the finished `dgxtop-core` contract; the lead integrated each crate into this tree
  and reran the owner gates on the integrated workspace. The integration suites
  (`tests/action_disposable`, `tests/fault_injection`), the benchmark harness and a read-only
  AT01–AT40 evidence map were produced the same way and reviewed before adoption.
- Decisions that refine the specification: [DECISIONS.md](DECISIONS.md) (D-01 … D-57).
- Defects found by the evidence map and the fault-injection suite and fixed with a failing test
  first: terminal hang-up spin (D-15), Manager fail-stop and dead-Manager sessions (D-19), event
  ring evicting control events for telemetry (AT30), ingress bytes ignored by the pressure
  signal (AT30), query memory shortage reported as `CapacityExceeded` instead of `Busy`
  (AT38), projections allocated before admission (D-21), lost audit rows for expired or revoked
  confirmations and outcomes written for missing intents (D-18), stripped instead of escaped
  audit names and split kernel names (D-20), hung-backend values shown as current (D-22),
  unreported archive gaps (D-23), unbounded total shutdown time (D-24), misleading "not
  changed" after uncertain commits (D-25, plus the uncertain-commit message fix in
  `crates/manager/src/config.rs`), exited selections vanishing silently (D-26). Two test
  harness races were fixed as well: the PTY harness leaked the PTY master into the child, and a
  supervisor test waited for host states in a fixed order. The network collector published a
  rate twice for one key when the key had several counter candidates (the store rejected the
  whole batch).
- Later defects, also fixed test-first:
  - the installer and the release gate accepted a version that only prefixed the binary's
    version;
  - the release gate did not check the artifact's SQLite runtime line;
  - build provenance ignored untracked files and unstaged edits (D-34);
  - REF coverage was taken before the last reads of the window had committed (D-33);
  - GPU cards repeated the "GPU" prefix of the driver label;
  - a refused adapter dispatch, which the scheduler retries, was counted as a dropped queue
    item, so REF reported an unexplained drop at 100 % coverage (D-37);
  - the cached CPU maximum frequency kept its discovery read time and went stale half a minute
    after start, so the CPU summary read `max stale` (D-42).
- REF CPU work (T12), measured with 60 s smoke runs and recorded in `docs/validation/`:
  - replay hosts serve live adapter families (D-27);
  - the generator honours the 2048-source count (D-28);
  - sampling phases are per adapter, with a bounded 25 ms collector hold (D-29);
  - projections follow data changes (D-30);
  - the hot path shares and interns data, and indexes the metric catalog (D-31);
  - an A/B of the overview redesign on 2026-10-05, with two other processes using 100 % CPU
    each (load average ≈ 2.4): 4.850 / 4.783 / 4.800 % before and 4.800 / 4.933 / 5.033 %
    after. The extra CPU is in code the redesign did not change (adapter hosts +0.08, hot
    store +0.05, supervisor +0.03 points). The changed Manager presentation and frontend used
    less or the same (−0.05 and 0.00 points). This reads as load noise, not a regression, but
    under load the margin to the 5 % budget is at most 0.2 points.
- Overview redesign from the owner's feedback on the running program (D-36), covering T10:
  - **Overview:** GPU cards lead and take the largest share. CPU, memory, disk and network
    sit in a summary strip. CPU starts collapsed to its total and expands by click or `c`.
  - **Memory and processes:** memory colors the RAM that GPU processes hold on unified-memory
    GPUs. The process table lists GPU processes only and is the single place for process
    actions.
  - **Tabs:** the Sources and Processes tabs are hidden.
  - **GPU view:** redesigned around gauges, grouped facts and side-by-side trends.
- Reference layout from the owner's screenshot (D-38): CPU and memory, then the GPUs with
  utilization, temperature and power charts, then disk I/O and network with rate charts, then
  the GPU processes. The tabs moved into the header row. The GPUs grow first unless the user
  expanded the CPU.
- Small overview charts (D-39): the owner could not see the disk and process tables at
  213×41 with two GPUs. The charts now take two rows each and the tables get the rest.
- Memory wider than the CPU, and GPU charts of utilization and VRAM instead of temperature
  and power (D-40); the Manager now records each GPU's memory in use.
- Charts for a quick read (D-41): the owner found the overview charts hard to read. Charts
  became block columns colored per column like the gauge; disk and network rates are scaled to
  their own peak, which is printed.
- Charts under the bars again, a little taller, and no VRAM chart on the GPUs (D-43). The
  Actions tab is gone; confirmations stay banners and outcomes notices (D-44). The GPU view's
  charts use the same style in a grid (D-45).
- History redesigned (D-46): the owner could not read it. It is now the overview's charts over
  5 minutes to 24 hours on one time axis, with the peak dimmed above each column's mean,
  instead of one series picked from a list of every core and entity.
- GPU view redesigned (D-47): the owner found it split into small cells, blank under the
  decoder, listing data the GPU does not have and missing what v1 showed. It now runs in
  full-width rows: state (P-state, throttle, PCIe link), clocks, four full-width charts with
  their facts in the captions (thresholds, energy), the 1 h–24 h statistics, the processes on
  the GPU and one footnote for everything the GPU does not report.
- GPU view with live and deep data, statistics in History (D-48):
  - **GPU view:** a bar per live reading, each with its facts and its chart under it, then a property
    sheet: state and throttle time, clocks, PCIe with its theoretical capacity and the driver's
    traffic, the memory bus and peak bandwidth, the CUDA stack (driver CUDA version, compute
    capability, toolkit, nvcc), device, driver, firmware and PCI identity.
  - **Sources:** all of it from NVML or from driver and toolkit files, without running a command.
  - **History:** the 1 h to 24 h statistics of every GPU moved to the history view, under its
    charts.
- GPU view as an information panel (D-49): the owner asked for no processes, a smaller status and
  the GPU's information as the core. The status is four bars without facts. The information
  follows in sections (performance, memory, thermal and power, PCIe, CUDA and driver, device,
  health) on one aligned grid of up to four columns, one group of related values per row. The
  charts take only the rows left over; a short terminal names the sections it leaves out.
- GPU view in blocks, history redesigned (D-50):
  - **GPU view:** live bars only. The information reads top to bottom in blocks (state,
    utilization, clocks, memory, PCIe, thermal, power, CUDA, driver, device, PCI & firmware,
    health), transmit over receive and each value now beside its maximum, in as few balanced
    columns as fit.
  - **History:** redesigned after an analysis of good history charts
    ([history-redesign.md](history-redesign.md)). Panels in pairs, the overview's resources
    over the window: CPU, memory, each GPU's utilization, memory, temperature and power, the
    disk and network pairs. Every strip has a scale, a legend with now, mean and max and a
    shared time axis. The long-term table no longer repeats numbers a window does not add.
- History kept on disk (D-51): the archive is on in the built-in defaults (the spec's default
  document, which REF uses, is unchanged). The history view reads the time before this run,
  earlier runs and boots, back from it and charts both on the wall clock.
- Bars and charts that never touch (D-52): bars stand three quarters of a row high and charts
  leave the top quarter of their top row empty; the GPU view has no notes; missing values are
  dim, never italic (tmux showed italic as reverse video).
- Fewer tests (D-53): the default run keeps the core cases and every cited acceptance check
  (1742 → 1351, 78 s → 18 s); the PTY, fault-injection and real-host suites run on request
  (`just e2e`), so the eight acceptance tests resting on them are NOT_RUN until then. Removed
  tests and reasons: [test-triage.md](test-triage.md).
- GPU view and process additions (D-54): a Topology block (nearest NUMA node and CPUs, the PCIe
  path and P2P access to each other GPU), the instantaneous power and the requested limit, every
  fan, and a `TYPE` column (`C`, `G`, `C+G`) in the process table. What an outside analysis
  proposed but the hardware or the design could not back (a throttle share, GPM, memory
  temperature, Xid, application data, a clock history panel) is listed in D-54 with the reason.
  The owner keeps the "throttled for" totals as they are and wants no clock panel (D-54).
- False "archive near a capacity threshold" status (fixed with a failing test first,
  2026-10-10): SQLite truncates a restarted WAL back to `journal_size_limit`, which equals the
  checkpoint threshold, so after the first checkpoint the WAL file stays at the threshold and the
  status reported the archive (and could report the audit) as degraded for good. The check
  now counts the frames readers keep from the checkpoint, as write admission already did.
- History without peak bands (D-57, 2026-10-10): the grey band behind each column's mean first
  floated with a dark gap, which a failing test reproduced and a fix closed. The owner still saw
  grey blocks, worst on bursty DGX loads, so the strips now draw the means only. Rate pairs
  scale to their tallest column, and the legend keeps the window's peak.
- Agent work: three worker agents for the remaining acceptance gaps were stopped by the owner
  before delivering. Their partial snapshots were not integrated, and the gaps they targeted
  are listed in `docs/validation/VALIDATION.md`.
- Quality gates: `just ci` (fmt, check, clippy `-D warnings`, test, dependency boundaries, script
  tests; all `--locked`). End-to-end suites on request: `just e2e`. Hardware suites:
  `just hardware`.

## Closed decisions (D-55)

- NVML PCIe throughput (D-11): the GPU view keeps the driver's own reading in "KB/s" and the
  canonical value stays unavailable. NVML's 32-bit PCIe byte counters wrap too often to replace
  it, and no calibration separates 1000 from 1024.
- The Sources tab stays removed (D-36). The owner authorized committing and pushing this work.

## Next action

The work continues on a DGX Spark: [handoff-dgx.md](handoff-dgx.md) has the state, the checks
waiting for that machine, how to record evidence and the owner's working rules.

1. **DGX Spark checks, on the owner's DGX machine:**
   - unified-memory RAM figure against `nvidia-smi` and `free -h`;
   - GB10 hwmon power mapping (D-10);
   - live hardware suites (`just hardware`).
2. **Remaining gaps** (see VALIDATION.md "Known gaps"):
   - metrics output (AT17);
   - IPC decode credit (AT21);
   - port-exclusivity checks;
   - the listed missing tests;
   - the soak driver.

   Then the long runs: REF 3 × 30 min and the 72 h soaks.
