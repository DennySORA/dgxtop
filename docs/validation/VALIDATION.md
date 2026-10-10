# dgxtop v2 — software validation report

This report records what was actually executed against the v2 rewrite and what was not. It
follows the rules of `spec/dgxtop-v2/ACCEPTANCE.md`:

- the only statuses are PASS, FAIL, NOT_RUN and BLOCKED;
- PASS needs executed evidence for every clause;
- written code is never a PASS;
- missing hardware or time is never a PASS.

`scripts/check_validation.py` (run by `just ci`, CI and the release gate) checks this file
against the evidence logs:

- every AT is present exactly once;
- every log hash and exit code matches;
- every test named for a PASS case passed in a cited log.

Specification acceptance (the spec package and its Python tools) and software acceptance (the
Rust program) are reported separately. The spec package's own Python tests are not Rust or
hardware evidence.

## Snapshot under test

- **Source:** the uncommitted v2 tree on top of commit `b4b5a8d` (branch `main`). No commit was
  authorized, so every log records `git_commit: b4b5a8d… (working tree modified)` and every
  binary reports `<commit>-dirty`. A source commit therefore cannot be identified, which on its
  own blocks any acceptance-grade benchmark report.
- **Lock file:** `Cargo.lock` SHA-256 `ebe26984a218038f7892b8eb12d3b607593e86d4460c07da1ce0276f5c1cea74`.
  It is unchanged throughout this work.
- **Dates:** the spec package validation ran on 2026-10-03 and every other log on 2026-10-09 to 2026-10-10
  (UTC). Each log carries its own timestamps.
- **Scope (D-53, `core-fast60`):** the gate logs (`gate-*`, `rules-preserved`,
  `spec-copy-verification`) and the release build were recorded on 2026-10-10 for this tree,
  the D-54 GPU view additions included. The end-to-end suites (PTY, fault injection, real host
  children) run only on request and were not run (user_opt_in_required): every acceptance test
  that rests on them is NOT_RUN. The hardware and benchmark logs are from the D-50 tree
  (2026-10-09) and were not run again for D-51 to D-54; no PASS row cites them. D-54's new NVML
  queries were checked against both GPUs with a read-only probe outside the test suites, not by
  the hardware suite.

## Environment

| Item | Value |
|---|---|
| CPU | Intel Core i9-13900K, 32 logical CPUs, frequency governor `powersave` |
| Memory / disk | 62 GiB RAM; repository and state on a local NVMe SSD (ext4) |
| OS | Debian GNU/Linux 13 (trixie), kernel 6.12.111+deb13-amd64, glibc 2.41 |
| Toolchain | rustc 1.92.0, cargo 1.92.0 (`rust-toolchain.toml`) |
| SQLite | bundled 3.53.2 through `rusqlite` 0.40.2 / `libsqlite3-sys` 0.38.2; runtime gate ≥ 3.51.3: ok |
| GPUs | NVIDIA GeForce RTX 5080 and RTX 3060, driver 595.104.02 |
| Python | 3.13.5. The spec tools ran with the pinned `jsonschema` from `spec/dgxtop-v2/validation/requirements.txt`, in a private virtual environment |
| Not available | DGX Spark (GB10), 72-hour run windows, ≥ 24 h host uptime at benchmark time, a CI or release run, profilers |

Platform status: x86_64 GNU/Linux with discrete NVIDIA GPUs has real-host adapter evidence, but
not acceptance-grade performance evidence. aarch64 / DGX Spark has no evidence. No platform is
called validated.

## Evidence logs

Each log was written by `scripts/record_evidence.sh <name> <command…>`. The script stores the
following in `docs/validation/evidence/<name>.log`:

- the command;
- UTC start and finish times;
- the git commit and whether the tree was dirty;
- the host, CPU and rustc;
- the full output;
- the exit code.

GPU UUIDs and serial numbers are masked before the file is hashed.

| Evidence | Command | Exit | SHA-256 |
|---|---|---|---|
| `gate-fmt` | `cargo fmt --all -- --check` | 0 | `0781aae44344d37d15e1137355f9b178ced00b53dbe0845b052c530257e6b4dd` |
| `gate-check` | `cargo check --workspace --all-targets --all-features --locked` | 0 | `a7daf2048a1410a9d1f76336b7f4091043a58cd096451fb40977bf43d1c0bcb5` |
| `gate-clippy` | `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | 0 | `a3f2d0458784d9217a4a661f239b6bcab2def2e6db88a2709b7d55f90bc4177a` |
| `gate-test` | `cargo test --workspace --all-features --locked --no-fail-fast` | 0 | `c5f347d770e83a47e018fa444f6c51151ea58e25bf3fce7906a36d1bd61dfbc2` |
| `gate-boundaries` | `bash -c mkdir -p target && cargo metadata --format-version 1 --locked > target/metadata.json && python3 spe…` | 0 | `96030e724e23c847530b02cd6645ebb0cc4aedae2800c84164e35170370947c9` |
| `gate-scripts` | `env PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p test_*.py -v` | 0 | `06d8d98a97d7e8f2c5f84b1874ef14a1f16cb220603065d624e49c789ee30003` |
| `rules-preserved` | `bash -c git ls-files --error-unmatch CLAUDE.md && git diff --exit-code HEAD -- CLAUDE.md && echo "CLAUDE.md…` | 0 | `155bded4c4e8dbdb5c85167666a68b1f8cfba59d28a450086b76e7bf7da3245e` |
| `hardware-suites` | `cargo test --workspace --all-features --locked -- --ignored hardware_ --test-threads=1` | 0 | `b3ca9c8fede1cf34e8e1e303cd8bf2f2a631b77e2cdb0739d3b4abac6f38edbd` |
| `spec-package-validation` | `/tmp/claude-1000/-mnt-DennySORA-SSD-Code-Github-dgxtop/32efa81a-0f25-482f-b209-eb95f51fb786/scratchpad/run_…` | 0 | `d28c5098c16fe2aa732aa9c1f556b2be374a1e6604997f65d14c33e61f00d817` |
| `spec-copy-verification` | `python3 scripts/verify_spec_copy.py dgxtop-spec-v2.zip spec/dgxtop-v2` | 0 | `071fb3d363aff58c1ede7e4e5f2d921a1f5c1fa8716c7bde328e4f600d17ae20` |
| `bench-ref-smoke-60s` | `env DGXTOP_BENCH_SMOKE_SECONDS=60 cargo bench -p dgxtop --locked --offline --bench ref -- --smoke` | 0 | `143f3c76df9e0bb87417d7dc2328148f5542ef76ec22708f6398337c520d24f8` |
| `bench-projection-smoke` | `cargo bench -p dgxtop --locked --offline --bench projection -- --smoke` | 0 | `6ab122ad7947c1456c8c4c38b870fcaa8e3b4937099a38ba303f0e5d379af276` |
| `bench-history-smoke` | `cargo bench -p dgxtop --locked --offline --bench history -- --smoke` | 0 | `2a00d13024a416b9a27091f74c540f4f7b07d372ba05239d7dea27881f71aefd` |
| `bench-arbitration-smoke` | `cargo bench -p dgxtop --locked --offline --bench arbitration -- --smoke` | 0 | `72fde55c2f27d1f865a7c50addf7cacf87c8fded76ce414137f4706887779ad0` |
| `release-build` | `cargo build --release --locked` | 0 | `177e1ca2e59245ea65eed61c8e2e4a00baaac49b6a29e104ba768162f3e283e6` |
| `release-version-verbose` | `target/release/dgxtop --version --verbose` | 0 | `477f73c0867df13fd7d43cd07124a1df68eb2d803dad4ae93a7b7cd0cbbb9634` |
| `gate-fmt-gb10` | `cargo fmt --all -- --check` | 0 | `5f569412cb303198e81f4207e2dd2868bc36118665dff64ecae4c8fb8aecb7cb` |
| `gate-check-gb10` | `cargo check --workspace --all-targets --all-features --locked` | 0 | `54754b35c5a2076ac4f2ba19ccdd6710291d1907601c4454a648ef5075b86fe7` |
| `gate-clippy-gb10` | `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | 0 | `2fc1582d50e36e760184001d1431c768c12484ad3c6df93830192ba892223ee4` |
| `gate-test-gb10` | `cargo test --workspace --all-features --locked --no-fail-fast` | 0 | `9ad8192d6dc334a813b0967bec0e96438479b9da0608c812226b6773a1c40067` |
| `gate-boundaries-gb10` | `bash -c mkdir -p target && cargo metadata --format-version 1 --locked > target/metadata.json && pyth…` | 0 | `9ff7f26c3a8adfbdd1c4b721dee1ba83056dd6f192f35aaa646d13e70fb368b2` |
| `gate-scripts-gb10` | `env PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p test_*.py -v` | 0 | `42a9f92b49f4ba016cd96b2c2348748e97ba7aa340d1709fdefb86a36215bd24` |
| `rules-preserved-gb10` | `bash -c git ls-files --error-unmatch CLAUDE.md && git diff --exit-code HEAD -- CLAUDE.md && echo "CL…` | 0 | `e298f794eef3f2ebe9c35c74be0dc12138eeddcadd1f69bc3a8fe0e98e7ffd6b` |
| `spec-copy-verification-gb10` | `python3 scripts/verify_spec_copy.py dgxtop-spec-v2.zip spec/dgxtop-v2` | 0 | `ae8fe867a1d521670e581ca26056a474a18cb0ad9243988a8cebe4e52e5e18e7` |
| `hardware-suites-gb10` | `cargo test --workspace --all-features --locked -- --ignored hardware_ --test-threads=1` | 0 | `7fdce046c815034f4f7ab5e9c5d5aecd9a856e45398253e0b86d0884fd6aabc0` |
| `release-build-gb10` | `cargo build --release --locked` | 0 | `c11483ac1e9ae85c67aa6888bdf1fc810aced7adaa43ee3f7fb847777283a1e9` |
| `release-version-verbose-gb10` | `target/release/dgxtop --version --verbose` | 0 | `36fc2e78e3506c7b4164bb548e889ac0e46a7b0e05650828d92e301ffabcd292` |
| `gpm-probe-gb10` | `nvidia-smi dmon --gpm-metrics 1,2,3,10 -c 3` | 0 | `42f3a548c475373840b015cc361181a7ab69be8b4c10e4ccf7d2f3d1ef1470b6` |
| `hwmon-probe-gb10` | `bash -c for h in /sys/class/hwmon/hwmon*; do echo "$h name=$(cat $h/name)"; ls $h | grep -E "^(power…` | 0 | `37720f845629dcc81b42e2add2216f2d4fae55f02443f7247302f9598f50077d` |

## Acceptance status

Totals: PASS 12, FAIL 0, NOT_RUN 24, BLOCKED 4 (of 40).

| AT | Requirement | Status | Evidence | Summary |
|---|---|---|---|---|
| [AT01](#at01) | R01 Adapter read/write interface isolation | NOT_RUN | `gate-test`, `gate-boundaries`, `hardware-suites` | Seven live adapters discover and read on a real host; port exclusivity is checked only by types |
| [AT02](#at02) | R02 Typed identity, units, time and provenance | PASS | `gate-test` | Exact u64, every key field, unit conversion and range checks tested |
| [AT03](#at03) | R03 Multi-source arbitration and de-duplication | PASS | `gate-test` | Stable selection, alias de-duplication, 3-win/5 s switching, conflicts as null |
| [AT04](#at04) | R04 Missing values, quality, kind and freshness kept apart | PASS | `gate-test` | True zero kept; failures, stale values and replay data never become zero or live history |
| [AT05](#at05) | R05 Modular Manager and a single logical writer | NOT_RUN | `gate-test` | Atomic commits and stale-batch rejection tested; port exclusivity is structural only |
| [AT06](#at06) | R06 Store partitions and local persistence faults | NOT_RUN | `gate-test` | Partition quotas visible; archive and audit faults stay local and never allow a signal; the end-to-end part is opt-in and was not run |
| [AT07](#at07) | R07 Real-time history, counters and coverage | NOT_RUN | `gate-test` | Exact counters, totals and coverage tested; counters across a UTC step are not |
| [AT08](#at08) | R08 Configuration precedence, CAS and live periods | PASS | `gate-test` | Explicit CLI overrides, CAS revisions, new epoch and effective interval, failures never reported as success |
| [AT09](#at09) | R09 ProcessKey sampled once, multi-GPU association | PASS | `gate-test` | One host row per ProcessKey with several GPU links; PID reuse and merged associations handled |
| [AT10](#at10) | R10 Process termination: authorization, stable identity, audit | PASS | `gate-test` | Real disposable child, real SQLite audit, real pidfd and Manager: one durable intent, at most one SIGTERM, every refusal sends nothing |
| [AT11](#at11) | R11 Controller only translates input intent | NOT_RUN | `gate-test` | Keys and clicks become typed commands; stable selection; quit and cancel reachable; no system calls; the end-to-end part is opt-in and was not run |
| [AT12](#at12) | R12 Viewer layout and module extension | NOT_RUN | `gate-test` | Layout tiers, registry extension and narrow screens pass; the sources module is registered but not offered (D-36) |
| [AT13](#at13) | R13 Immutable snapshots, stable selection, stale frames | NOT_RUN | `gate-test` | Latest-only slots, dirty-only drawing and key-based selection tested; immutability is type-level only |
| [AT14](#at14) | R14 Worker isolation, timeouts, backpressure, reaping limits | NOT_RUN | `gate-test` | A hung host is isolated, timed out, restarted only after reap, quarantined when unreapable; the end-to-end part is opt-in and was not run |
| [AT15](#at15) | R15 Discovery, hot-plug and capability updates stay local | PASS | `gate-test` | 1000 hot-plug rounds bounded; new incarnations; static metadata cached; partial pages never delete |
| [AT16](#at16) | R16 Source diagnostics, age and visible exceptions | NOT_RUN | `gate-test` | Diagnostics are built and tested, but the Sources view is not offered (D-36) |
| [AT17](#at17) | R17 Observability, privacy and capacity targets | NOT_RUN | `gate-test` | Bounded labels, escaped audit names and log drop counters tested; the metrics are not output and nine are never recorded |
| [AT18](#at18) | R18 Staged migration and recoverable program/configuration | NOT_RUN | `gate-test` | Legacy dry-run/apply, rollback and corrupt-file handling tested; installer disk-full and interrupted switch are not |
| [AT19](#at19) | R19 Existing rules kept, real verification records | NOT_RUN | `rules-preserved`, `spec-package-validation` | Rules file unchanged and evidence recorded; the owner's manual check is not recorded |
| [AT20](#at20) | R20 Reproducible build, version and install integrity | BLOCKED | `gate-test` | Installer checks pass locally; the tag gate, locked CI build and tracked Cargo.lock need a commit and a release run |
| [AT21](#at21) | R21 Global budget and dual queue quotas | NOT_RUN | `gate-test` | Ledger, dual gate and reservation tests pass; IPC decode still allocates before its decoded-size check |
| [AT22](#at22) | R22 Multi-rate scheduling, broker de-duplication, epochs | NOT_RUN | `gate-test` | 1 s / 5 s / 10 s / 60 s cadences, one read per group and cycle, no catch-up storms, stale replies dropped; the end-to-end part is opt-in and was not run |
| [AT23](#at23) | R23 Fixed workers and a fair control loop | NOT_RUN | `gate-test` | Fixed pools, fair iterations and Busy replies tested; the 2 ms slice bound has no executed check |
| [AT24](#at24) | R24 Blocked hosts and the quarantine limit | NOT_RUN | `gate-test` | A hung GPU host is isolated and restarted only after reap; signalling only own hosts is structural |
| [AT25](#at25) | R25 Incremental projection and bounded snapshot lifetime | NOT_RUN | `gate-test` | Visible-topic projections, the 1 MiB cap and latest-only slots tested; two slow sessions are not |
| [AT26](#at26) | R26 Non-blocking terminal and backpressure | NOT_RUN | `gate-test` | Complete frames, full redraw after a slow frame, q works with a reader stalled 30 s, bounded buffer; the end-to-end part is opt-in and was not run |
| [AT27](#at27) | R27 History window accuracy and cross-bucket counters | PASS | `gate-test` | Cross-minute counter bridges counted once, boundary gaps instead of proportional guesses, numeric ordering |
| [AT28](#at28) | R28 REF performance and coverage thresholds | BLOCKED | `gate-test`, `bench-ref-smoke-60s` | 60 s smoke runs pass every budget; the 3 × 1800 s runs need an identifiable commit and ≥ 24 h uptime |
| [AT29](#at29) | R29 72-hour stability and source churn | BLOCKED |  | No 72 h windows here, and no soak driver exists yet (results can only be imported) |
| [AT30](#at30) | R30 Overload state machine and observable degradation | NOT_RUN | `gate-test` | Pressure at 75 % for three samples, throttling at 90 % memory, one step down per healthy 30 s, no flapping; the end-to-end part is opt-in and was not run |
| [AT31](#at31) | R31 SQLite version, WAL and disk limits | NOT_RUN | `gate-test` | Version gate, WAL modes, checkpoints and low-disk refusal tested; a real SQLITE_FULL on the intent write is not |
| [AT32](#at32) | R32 Late audit results, cancellation, at-most-once action | PASS | `gate-test` | Single-use tokens, linearized send/cancel, TTL and policy re-checked after the durable receipt, unknown never resent |
| [AT33](#at33) | R33 IPC length, depth, generation and decode limits | NOT_RUN | `gate-test` | Split prefixes and UTF-8, 256 records, depth 32, oversized prefixes rejected without allocating, identity checks; the end-to-end part is opt-in and was not run |
| [AT34](#at34) | R34 Catalog cache and unsupported-probe throttling | PASS | `gate-test` | Metadata cached per generation, unsupported re-probed at most once per 60 s, no nvidia-smi |
| [AT35](#at35) | R35 Replayable fixtures and benchmark evidence | NOT_RUN | `gate-test`, `spec-package-validation` | Deterministic fixtures in their own namespace; synthetic and wrong-workload reports cannot be measured; metrics are not recomputed from raw files |
| [AT36](#at36) | R36 Uncertain outcomes of versioned configuration persistence | PASS | `gate-test` | Atomic write, rename and directory sync; commit_uncertain reloads the disk version; no concurrent overwrite |
| [AT37](#at37) | R37 Release gate separate from hardware evidence | BLOCKED | `gate-test` | Hardware suites are ignored in normal CI and no platform is claimed validated; CI, release and DGX Spark evidence do not exist yet |
| [AT38](#at38) | R38 Bounded cardinality, query and NSS cost | PASS | `gate-test` | Point, scan and byte caps; two history workers; numeric UIDs; Busy under memory shortage; bounded pinning |
| [AT39](#at39) | R39 Layered errors and safe shutdown | NOT_RUN | `gate-test` | Read failures stay local, Manager fail-stop sends nothing, bounded restore and 3 s shutdown, stuck storage never locks the loop; the end-to-end part is opt-in and was not run |
| [AT40](#at40) | R40 Concrete performance evidence and full specification validation | NOT_RUN | `spec-package-validation`, `spec-copy-verification`, `gate-scripts` | Spec copy verified (CRC, bytes, MANIFEST) and this report is machine-checked; the spec's browser HTML checks were not run |

## Details

Clause labels follow the order of the clauses in `ACCEPTANCE.md` (S = success path, F =
failure or edge case). Test identifiers name the package (crate tests), `package/target`
(crate integration tests) or the workspace suite (`tests/<suite>`), then the test function.

<a id="at01"></a>

### AT01 — R01 Adapter read/write interface isolation

**Status: NOT_RUN.** Evidence: `gate-test`, `gate-boundaries`, `hardware-suites`. S1 and S3 rest on Rust types with no executed check that a collector or another service cannot receive the action or store port. F2 is shown only with GPU monitoring disabled or a hung GPU host, not with NVML absent.

| Clause | Tests | Note |
|---|---|---|
| S1 a collector holds only the telemetry port | `contracts::structure::implementation_crates_depend_on_no_workspace_crate_but_core`, `contracts::structure::collectors_manager_and_viewer_never_touch_files_processes_or_sockets`, `dgxtop-collectors::registry::tests::registry_holds_the_six_collectors_by_id` | STRUCTURAL: collectors are built without ports; no name scan executed |
| S2 seven live adapters discover and read | `dgxtop-adapters/adapters_a_registry::live_adapter_list_is_the_naming_contract`, `dgxtop-adapters/adapters_a_registry::every_live_adapter_can_be_built_with_its_options`, `dgxtop-adapters/adapters_a_procfs::cpu_read_emits_counters_load_and_frequencies`, `dgxtop-adapters/adapters_b_nvml::healthy_gpu_reports_native_values_with_uuid_identity`, `hardware::hardware_every_live_adapter_discovers_and_reads_through_isolated_hosts` | Real host: `hardware-suites` (x86_64 workstation, not DGX Spark) |
| S3 the process-action port only reaches ProcessService |  | STRUCTURAL: constructor types only |
| F1 the boundary check rejects action/store edges into collectors | `contracts::structure::implementation_crates_depend_on_no_workspace_crate_but_core` | Also `gate-boundaries` (`check_boundaries.py`) |
| F2 no NVML: system metrics stay available | `dgxtop-adapters/adapters_b_nvml::init_failure_reports_unavailable_with_every_capability_unsupported`, `dgxtop-adapters/adapters_b_nvml::no_gpu_reports_unavailable`, `pty::live_mode_without_gpu_starts_isolated_hosts_and_quits` | PARTIAL: the live run disables GPU monitoring; NVML missing is not run end to end |

<a id="at02"></a>

### AT02 — R02 Typed identity, units, time and provenance

**Status: PASS.** Evidence: `gate-test`. Every clause has a passing unit, property or contract test.

| Clause | Tests | Note |
|---|---|---|
| S1 u64::MAX round-trips exactly | `dgxtop-core::wire::tests::max_u64_round_trips_exactly`, `dgxtop-core::wire::tests::decimal_round_trip`, `dgxtop-core::metric::tests::scalar_json_shape_matches_schema`, `dgxtop-core::identity::tests::process_key_json_uses_decimal_start_time`, `contracts::u64_values_are_decimal_strings_up_to_max` | Property test included |
| S2 keys differ when any field differs | `dgxtop-core::identity::tests::every_metric_key_field_participates_in_equality`, `dgxtop-core::identity::tests::process_key_fields_all_matter` | All 5 MetricKey and all 5 ProcessKey fields varied |
| S3 mdegC → 68 °C | `dgxtop-core::metric::tests::millidegrees_convert_to_68_degrees` |  |
| F1 u64 overflow rejected | `dgxtop-core::wire::tests::rejects_overflow_sign_and_leading_zero`, `dgxtop-core::metric::tests::exact_u64_conversion_and_overflow` |  |
| F2 NaN/Inf rejected | `dgxtop-core::metric::tests::non_finite_scalars_are_rejected` |  |
| F3 unknown unit rejected | `dgxtop-core::metric::tests::unknown_and_unverified_units_are_rejected` |  |
| F4 PID ≤ 0 or > i32::MAX rejected | `dgxtop-core::identity::tests::pid_range_is_enforced`, `contracts::invalid_pids_are_rejected_by_rust_and_schema` | Rust types and JSON Schema |
| F5 different scopes never merged | `dgxtop-core::identity::tests::different_scope_is_different_key`, `dgxtop-collectors::arbitration::tests::soc_scope_is_a_different_key_and_never_competes`, `dgxtop-collectors::cpu::tests::architecture_example_soc_scope_is_published_separately`, `replay::package_temperature_alias_counts_once_and_soc_stays_separate` |  |

<a id="at03"></a>

### AT03 — R03 Multi-source arbitration and de-duplication

**Status: PASS.** Evidence: `gate-test`. Every clause has a passing test; inputs are in-memory fixtures, not real sensors.

| Clause | Tests | Note |
|---|---|---|
| S1 stable selection within tolerance | `dgxtop-collectors::arbitration::tests::{agreeing_independent_sources_select_by_profile_rank, outcome_is_independent_of_candidate_order}` | Order independence is a property test |
| S2 an alias counts once | `dgxtop-collectors::arbitration::tests::confirmed_alias_is_counted_once_and_keeps_an_inconsistency_hint`, `replay::package_temperature_alias_counts_once_and_soc_stays_separate` |  |
| S3 switching needs 3 wins and 5 s | `dgxtop-collectors::arbitration::tests::healthy_source_switches_only_after_three_wins_and_five_seconds`, `dgxtop-collectors::arbitration::hysteresis::tests::challenger_needs_three_wins_and_five_seconds` |  |
| F1 68 °C package and 42 °C SoC stay apart | `dgxtop-collectors::arbitration::tests::soc_scope_is_a_different_key_and_never_competes`, `dgxtop-collectors::cpu::tests::architecture_example_soc_scope_is_published_separately` |  |
| F2 a conflict has value null | `dgxtop-collectors::arbitration::tests::independent_disagreement_is_a_conflict_never_an_average`, `replay::independent_sources_beyond_tolerance_are_a_conflict_without_value` |  |
| F3 a pin never overrides a fault or conflict | `dgxtop-collectors::arbitration::tests::{unusable_pin_is_unavailable_and_never_silently_replaced, pin_never_hides_a_conflict}` |  |

<a id="at04"></a>

### AT04 — R04 Missing values, quality, kind and freshness kept apart

**Status: PASS.** Evidence: `gate-test`. Every clause has a passing test.

| Clause | Tests | Note |
|---|---|---|
| S1 a true 0 stays 0 | `dgxtop-collectors::arbitration::tests::true_zero_is_a_value_not_a_missing_sentinel`, `dgxtop-adapters/adapters_c_hwmon::zero_celsius_is_kept_as_a_valid_reading`, `dgxtop-manager::presentation::cell::tests::true_zero_is_a_value_not_missing` |  |
| S2 read fresh, measurement time unknown | `dgxtop-collectors::arbitration::tests::missing_device_timestamp_means_read_fresh_measurement_unknown` |  |
| F1 timeout/fault/unsupported/stale never 0 | `dgxtop-collectors::arbitration::tests::{failed_reads_and_stale_values_are_never_zero, faulted_fallback_is_excluded_and_the_valid_source_selected}`, `dgxtop-ui::format::cell::tests::missing_states_render_their_labels_never_zero` |  |
| F2 an alarm is not a fault | `dgxtop-collectors::arbitration::tests::alarm_keeps_the_hot_reading`, `dgxtop-adapters/adapters_c_hwmon::alarm_flag_keeps_high_reading_and_quality` |  |
| F3 replay never enters live history | `dgxtop-collectors::arbitration::tests::replay_is_rejected_by_a_live_policy_and_accepted_by_a_replay_policy`, `dgxtop-collectors::network::tests::replayed_counters_feed_no_live_interval`, `dgxtop-store/store_hot_commit::at05_invalid_namespace_unit_and_unknown_metrics_reject_the_whole_batch` | The live store rejects simulated values |

<a id="at05"></a>

### AT05 — R05 Modular Manager and a single logical writer

**Status: NOT_RUN.** Evidence: `gate-test`. S3 (services hold only the ports they need) and F3 (the UI holds no store writer) are enforced by Rust types and crate edges; no executed check names them.

| Clause | Tests | Note |
|---|---|---|
| S1 one commit carries canonical, history and state | `dgxtop-store/store_hot_commit::at05_commit_applies_samples_history_and_state_atomically`, `dgxtop-store/store_hot_commit::at05_capacity_rejection_leaves_no_partial_state` |  |
| S2 a collector baseline advances only after the receipt | `dgxtop-manager::control::tests::at05_pages_accumulate_into_one_job_and_state_advances_only_after_the_ack`, `dgxtop-manager::ingestion::tests::one_job_in_flight_and_backlog_coalesces_into_the_next_job`, `dgxtop-store/store_hot_commit::at05_stale_base_rejects_the_whole_batch` | The Manager side uses a fake store |
| S3 services hold only the ports they need |  | STRUCTURAL (`ControlCore::new` hands each service its own ports) |
| F1 a stale epoch or base rejects the whole batch | `dgxtop-store/store_hot_commit::at05_stale_epoch_rejects_the_whole_batch_and_changes_nothing`, `dgxtop-store/store_hot_commit::at05_stale_base_rejects_the_whole_batch` |  |
| F2 a resent batch never duplicates history | `dgxtop-store/store_hot_commit::at05_duplicate_batch_returns_the_original_ack_without_duplicate_history` |  |
| F3 the UI holds no store writer | `contracts::structure::implementation_crates_depend_on_no_workspace_crate_but_core` | PARTIAL: the UI crate cannot reach the store crate; nothing checks the core trait name |

<a id="at06"></a>

### AT06 — R06 Store partitions and local persistence faults

**Status: NOT_RUN.** Evidence: `gate-test`. F1, F2 and F3 also rest on the fault-injection rig, an opt-in end-to-end suite since D-53 that was not run for this snapshot (user_opt_in_required); F3 has no other test. Disk-full is injected through the free-space probe and a storage gate; the stuck-storage phases are real.

| Clause | Tests | Note |
|---|---|---|
| S1 every partition's quota is queryable | `dgxtop-store/store_hot_commit::at06_partition_usage_is_visible_in_store_health`, `dgxtop-store/store_hot_memory::eviction_under_a_tiny_budget_never_exceeds_partitions_and_reports_truncation`, `dgxtop-store/store_persist_audit::database_files_are_private`, `dgxtop-store/store_persist_archive::batch_insert_writes_every_table_in_bounded_transactions`, `dgxtop-store/store_persist_config::cas_success_increments_revision_with_private_modes` | Memory partitions report usage and limit; audit and archive report db+wal+shm against their limits |
| S2 archive off does not affect the hot store | `dgxtop-store/store_hot_commit::at06_archive_disabled_does_not_affect_the_hot_store` |  |
| F1 archive full: current values keep refreshing | `fault_injection::store_partitions::at06_stuck_then_full_archive_never_stalls_hot_store_commits`, `dgxtop-store/store_persist_archive::low_free_disk_refuses_appends_and_recovers`, `dgxtop-manager::archive::tests::gaps_are_counted_by_bounded_cause_and_made_visible` | Real hot store and SQLite archive actors |
| F2 audit cannot persist: no signal | `action_disposable::at10::at10_audit_refusing_the_intent_sends_nothing`, `action_disposable::at10::at10_unavailable_audit_store_sends_nothing`, `fault_injection::store_partitions::at06_at31_audit_below_low_water_refuses_actions_before_the_action_worker`, `fault_injection::store_partitions::at06_at31_low_water_after_prepare_refuses_the_intent_and_recovers`, `dgxtop-manager::process::tests::intent_write_failure_never_sends` | Real Manager, SQLite audit, pidfd and disposable child |
| F3 the Manager never stalls | `fault_injection::store_partitions::at06_at39_stuck_audit_times_out_the_intent_and_its_late_receipt_never_sends`, `fault_injection::safe_shutdown::at39_shutdown_with_hung_config_and_audit_storage_is_bounded_and_honest` | Views answered in < 1 s while audit storage hangs |

<a id="at07"></a>

### AT07 — R07 Real-time history, counters and coverage

**Status: NOT_RUN.** Evidence: `gate-test`. F3 is only half covered: a UTC step is a discontinuity for gauges, and counter maths uses the boot clock, but no test runs counter intervals across a UTC step.

| Clause | Tests | Note |
|---|---|---|
| S1 1000 → 1300 B in 3 s = 100 B/s, 300 total | `dgxtop-collectors::counter::tests::bytes_1000_to_1300_over_3s_is_100_per_second_and_300_total`, `dgxtop-collectors::network::tests::byte_rate_uses_actual_elapsed_time_and_emits_exact_interval`, `dgxtop-store/store_hot_history::at07_counter_interval_total_is_exact` |  |
| S2 changing the sampling rate keeps old totals | `dgxtop-store/store_hot_history::at07_changing_the_sampling_interval_keeps_old_totals`, `dgxtop-store/store_hot_properties::totals_of_disjoint_windows_add_up_to_their_union` |  |
| S3 coverage is exact | `dgxtop-store/store_hot_history::at07_coverage_is_exact_and_gaps_are_never_zero_filled`, `dgxtop-store/store_hot_properties::gauge_time_weighted_mean_matches_the_reference` |  |
| F1 counter reset | `dgxtop-collectors::counter::tests::reset_generation_boot_and_interval_changes_are_gaps`, `dgxtop-collectors::network::tests::counter_reset_is_never_a_negative_or_zero_rate` |  |
| F2 generation change | `dgxtop-collectors::network::tests::recreated_interface_restarts_from_warmup`, `dgxtop-store/store_hot_history::at07_generation_changes_cut_segments_and_utc_steps_are_discontinuities` |  |
| F3 UTC going backwards | `dgxtop-store/store_hot_history::at07_generation_changes_cut_segments_and_utc_steps_are_discontinuities` | PARTIAL: gauges only |
| F4 a different boot | `dgxtop-collectors::tests::boot_change_restarts_counters_at_warmup`, `dgxtop-store/store_hot_history::at07_other_boots_are_separate_series_and_unknown_boots_are_empty` |  |
| F5 a 2 h gap stays out of the last hour | `dgxtop-store/store_hot_history::at07_two_hour_gap_does_not_leak_into_the_last_hour` |  |

<a id="at08"></a>

### AT08 — R08 Configuration precedence, CAS and live periods

**Status: PASS.** Evidence: `gate-test`. Every clause has a passing test; the Manager side uses a fake configuration port, the repository side the real file system.

| Clause | Tests | Note |
|---|---|---|
| S1 CLI overrides explicit flags only | `dgxtop-core::config::tests::cli_overrides_only_touch_explicit_fields`, `cli::{check_config_writes_nothing_and_shows_explicit_overrides_only, saved_value_survives_absent_cli_flags}` |  |
| S2 CAS current → current + 1 | `dgxtop-store/store_persist_config::cas_success_increments_revision_with_private_modes` |  |
| S3 new epoch, effective interval visible | `dgxtop-manager::control::tests::at22_at08_late_reply_of_the_old_epoch_is_dropped_after_a_config_change` |  |
| F1 an old revision conflicts | `dgxtop-store/store_persist_config::stale_revision_conflicts`, `dgxtop-manager::config::tests::old_revision_conflicts_and_pending_change_is_busy` |  |
| F2 a persist failure never reports success | `dgxtop-store/store_persist_config::a_failure_before_the_rename_keeps_the_old_file`, `dgxtop-manager::control::tests::at08_persist_failure_keeps_the_running_configuration` |  |
| F3 restart-only changes return RequiresRestart | `dgxtop-manager::config::tests::restart_only_changes_require_restart_without_persisting`, `dgxtop-manager::control::tests::at08_restart_only_and_stale_revisions_never_persist` |  |
| F4 a late batch of the old epoch is ignored | `dgxtop-manager::control::tests::at22_at08_late_reply_of_the_old_epoch_is_dropped_after_a_config_change`, `dgxtop-manager::ingestion::tests::old_epoch_inputs_never_reach_a_job_and_expired_ones_become_gaps` |  |

<a id="at09"></a>

### AT09 — R09 ProcessKey sampled once, multi-GPU association

**Status: PASS.** Evidence: `gate-test`. Every clause has a passing test. S2 is shown by composition; F3 follows D-26. Since D-36 the table lists only processes with a GPU association.

| Clause | Tests | Note |
|---|---|---|
| S1 same host CPU/RSS across two GPUs | `dgxtop-collectors::process::tests::one_process_on_two_gpus_is_one_host_row_with_two_links`, `dgxtop-ui::viewer::tests::processes_view_follows_the_reference_columns_and_marks_the_selection` |  |
| S2 host data read once per cycle | `dgxtop-manager::scheduler::tests::cadences_follow_period_classes_and_discovery_interval`, `dgxtop-adapters/adapters_a_process::paging_is_consistent_within_a_cycle_and_resnapshots_the_next_cycle`, `dgxtop-collectors::process::tests::one_process_on_two_gpus_is_one_host_row_with_two_links` | Composition |
| F1 new start ticks: no inherited CPU base | `dgxtop-collectors::process::tests::reused_pid_with_new_start_time_never_inherits_the_old_base`, `dgxtop-collectors::process::tests::gpu_entry_of_an_older_instance_is_not_linked_to_a_reused_pid` |  |
| F2 graphics + compute not double-counted | `dgxtop-collectors::process::tests::merged_compute_and_graphics_association_is_one_link`, `adapters_b_nvml::compute_and_graphics_of_one_pid_are_one_association` |  |
| F3 an exited process is marked unavailable | `dgxtop-manager::presentation::processes::tests::selection_follows_the_process_key_when_rows_reorder`, `dgxtop-manager::presentation::processes::tests::a_selection_hidden_by_a_filter_is_not_reported_as_exited`, `dgxtop-manager::presentation::processes::tests::only_processes_using_a_gpu_are_listed`, `dgxtop-ui::modules::processes::tests::an_exited_selection_is_reported_as_unavailable` | D-26, D-36 |

<a id="at10"></a>

### AT10 — R10 Process termination: authorization, stable identity, audit

**Status: PASS.** Evidence: `gate-test`. Every clause has a passing end-to-end test on a disposable child created by the test.

| Clause | Tests | Note |
|---|---|---|
| S1 own child, held pidfd, valid confirmation → one durable intent | `action_disposable::at10::at10_confirmed_sigterm_reaches_only_the_disposable_child_after_one_durable_intent` | Order intent → send → syscall asserted |
| S2 at most one SIGTERM | `action_disposable::at10::at10_confirmed_sigterm_reaches_only_the_disposable_child_after_one_durable_intent`, `action_disposable::at32::at32_token_is_single_use_across_sessions_concurrency_and_replays` |  |
| S3 sent and exit shown separately | `action_disposable::at32::at32_signal_sent_is_reported_without_waiting_for_exit`, `dgxtop-ui::viewer::tests::actions_view_never_says_terminated` |  |
| F1 root session | `action_disposable::at10::at10_root_session_sends_nothing` |  |
| F2 another UID | `action_disposable::at10::at10_target_listed_under_another_uid_sends_nothing`, `action_disposable::at10::at10_process_of_another_user_is_refused_by_the_kernel_identity_check` |  |
| F3 another PID namespace | `action_disposable::at10::at10_target_in_another_pid_namespace_sends_nothing` |  |
| F4 PID reuse | `action_disposable::at10::at10_reused_pid_with_another_start_time_sends_nothing`, `action_disposable::at10::at10_target_gone_after_prepare_is_never_signalled_through_its_pidfd` |  |
| F5 expired confirmation | `action_disposable::at10::at10_expired_confirmation_sends_nothing` |  |
| F6 read-only | `action_disposable::at10::at10_read_only_or_disabled_actions_send_nothing` |  |
| F7 no pidfd support | `action_disposable::at10::at10_without_pidfd_support_nothing_is_sent_and_there_is_no_pid_fallback` |  |
| F8 no audit | `action_disposable::at10::at10_unavailable_audit_store_sends_nothing`, `action_disposable::at10::at10_audit_refusing_the_intent_sends_nothing` |  |
| F9 no bare-PID fallback | `action_disposable::at10::at10_bare_pid_targets_cannot_be_expressed_as_commands` |  |

<a id="at11"></a>

### AT11 — R11 Controller only translates input intent

**Status: NOT_RUN.** Evidence: `gate-test`. S3's real-terminal checks (`q` and Ctrl-C on a PTY) are opt-in end-to-end tests since D-53 and were not run for this snapshot (user_opt_in_required); the other clauses pass. F3 rests on the executed structure tests. Expanding the CPU panel (D-36) is presentation state and sends nothing.

| Clause | Tests | Note |
|---|---|---|
| S1 input becomes typed commands only | `dgxtop-ui::controller::tests::k_builds_prepare_signal_with_the_full_process_key`, `dgxtop-ui::controller::tests::tab_clicks_and_keys_switch_views`, `dgxtop-ui::controller::tests::filter_enter_applies_and_esc_clears`, `dgxtop-ui::controller::tests::the_cpu_panel_expands_and_collapses_by_click_or_c_without_a_command` |  |
| S2 j/k select a stable entity | `dgxtop-ui::controller::tests::j_and_k_select_by_process_key_across_a_reordered_projection` |  |
| S3 q and cancel are reachable | `dgxtop-ui::controller::tests::q_and_ctrl_c_quit_in_normal_mode`, `dgxtop-ui::controller::tests::enter_confirms_and_esc_or_n_cancel`, `pty::replay_starts_shows_simulated_label_and_quits_with_q`, `pty::ctrl_c_quits_even_in_filter_mode_where_q_is_text` | Real PTY |
| F1 K is text while filtering | `dgxtop-ui::controller::tests::in_filter_mode_k_and_q_are_text_while_ctrl_c_quits`, `dgxtop-ui::controller::keymap::tests::terminate_only_binds_in_process_views_in_normal_mode` |  |
| F2 a stale HitMap cannot act on another row | `dgxtop-ui::controller::tests::a_click_on_a_stale_hitmap_keeps_the_clicked_identity_and_old_revision`, `dgxtop-manager::views::tests::selection_survives_recent_frames_and_stale_missing_selection_is_rejected` | D-35 |
| F3 the Controller makes no system calls | `contracts::structure::collectors_manager_and_viewer_never_touch_files_processes_or_sockets`, `contracts::structure::core_and_the_viewer_carry_no_system_or_driver_dependencies` | Plus `#![forbid(unsafe_code)]` in dgxtop-ui |

<a id="at12"></a>

### AT12 — R12 Viewer layout and module extension

**Status: NOT_RUN.** Evidence: `gate-test`. All ten modules are registered and render in tests, but by the owner's decision (D-36) the source diagnostics view is not offered, so S1's "all ten modules usable" is not met from the interface.

| Clause | Tests | Note |
|---|---|---|
| S1 all ten modules usable | `dgxtop-ui::registry::tests::builtin_registers_the_ten_modules_without_warnings`, `dgxtop-ui::viewer::tests::every_view_renders_at_every_size_with_a_quit_hint`, `dgxtop-ui::viewer::tests::overview_follows_the_reference_layout_and_keeps_every_module_visible_when_wide`, `dgxtop-ui::views::tests::sources_processes_and_actions_are_not_offered` | NOT MET in the interface: sources and the action record are hidden at the owner's request (a pending confirmation is a banner in every view and an outcome a notice, D-44); the process table lives in the overview |
| S2 an eleventh module registers | `dgxtop-ui::viewer::tests::an_eleventh_fixture_module_is_registered_and_laid_out`, `dgxtop-ui::registry::tests::an_eleventh_module_can_be_registered`, `dgxtop-ui::viewer::layout::tests::a_module_without_a_cell_gets_a_full_width_row_below_the_grid` | A module without a grid cell still gets a row (D-38) |
| S3 ≥ 120 / 80 / < 80 / < 40 render | `dgxtop-ui::viewer::tests::every_view_renders_at_every_size_with_a_quit_hint`, `dgxtop-ui::viewer::tests::tiers_are_reported_per_size`, `dgxtop-ui::viewer::layout::tests::tiers_follow_the_spec_boundaries`, `pty::tiny_terminal_shows_size_notice_and_still_quits` |  |
| F1 duplicate id or API mismatch rejected | `dgxtop-ui::registry::tests::duplicate_ids_are_rejected_with_a_warning`, `dgxtop-ui::registry::tests::api_version_mismatch_is_rejected_with_a_warning` |  |
| F2 no GPU: unsupported shown, startup not blocked | `pty::live_mode_without_gpu_starts_isolated_hosts_and_quits`, `dgxtop-manager::presentation::gpu::tests::reasons_when_gpu_data_is_absent`, `dgxtop-ui::viewer::tests::gpu_unavailable_reason_is_shown`, `dgxtop-adapters/adapters_b_nvml::no_gpu_reports_unavailable` |  |
| F3 narrow screens keep quit and confirm | `dgxtop-ui::viewer::tests::confirmation_banner_is_visible_and_never_clipped_at_40x10` |  |

<a id="at13"></a>

### AT13 — R13 Immutable snapshots, stable selection, stale frames

**Status: NOT_RUN.** Evidence: `gate-test`. S1 (a published projection never changes) rests on the type (`Arc<ViewModel>` without interior mutability); no executed test holds a projection across later publishes. F2 is met as D-35 reads it.

| Clause | Tests | Note |
|---|---|---|
| S1 a projection is immutable | `dgxtop-manager::presentation::history::tests::small_results_are_shared_unchanged` | PARTIAL: type-level |
| S2 latest-only: 1 pending + 1 active | `dgxtop-core::view::tests::slot_keeps_only_latest_and_releases_bytes`, `dgxtop-manager::projection::tests::published_projection_holds_its_bytes_until_dropped`, `dgxtop-ui::terminal::runner::tests::input_and_q_still_work_while_output_is_blocked_for_30_seconds` |  |
| S3 nothing dirty → no draw | `dgxtop-ui::terminal::runner::tests::rendering_is_dirty_only`, `dgxtop-manager::views::tests::frames_are_rate_limited_and_viewport_changes_mark_dirty` |  |
| S4 selection kept by ProcessKey | `dgxtop-manager::presentation::processes::tests::selection_follows_the_process_key_when_rows_reorder` |  |
| F1 an old Arc stays accounted | `dgxtop-core::view::tests::slot_keeps_only_latest_and_releases_bytes`, `dgxtop-store/store_hot_memory::at13_snapshots_held_by_a_slow_reader_stay_accounted` |  |
| F2 a stale destructive frame is rejected | `dgxtop-manager::control::tests::at13_absent_or_unselected_targets_are_rejected_and_never_retargeted`, `dgxtop-manager::views::tests::destructive_targets_must_exist_and_match_the_selection` | D-35 |
| F3 full snapshots do not accumulate | `dgxtop-store/store_hot_memory::at25_the_store_retains_only_the_latest_snapshot` |  |

<a id="at14"></a>

### AT14 — R14 Worker isolation, timeouts, backpressure, reaping limits

**Status: NOT_RUN.** Evidence: `gate-test`. The real-child checks of S1, S2, F1, F2, F4 and F5 (scaled timings) are in the supervisor host suite, opt-in since D-53 and not run for this snapshot (user_opt_in_required); F1 and F2 have no other test. Some clauses are shown by composition.

| Clause | Tests | Note |
|---|---|---|
| S1 other hosts and commands continue | `dgxtop-runtime/supervisor_hosts::other_host_keeps_serving_while_one_hangs`, `dgxtop-manager/control_thread::at23_flood_from_another_thread_is_busy_and_every_accepted_command_is_answered` |  |
| S2 timeout → stale and circuit breaker | `dgxtop-runtime/supervisor_hosts::{hung_host_times_out_and_is_restarted_with_sigterm, circuit_backoff_follows_the_deterministic_schedule}`, `dgxtop-runtime::supervisor::circuit::tests::schedule_doubles_to_the_cap_within_ten_percent`, `dgxtop-manager::scheduler::tests::unavailable_host_reports_missed_slots_and_expired_requests` |  |
| F1 unreapable child → quarantine | `dgxtop-runtime/supervisor_hosts::unreaped_child_is_quarantined_and_never_respawned` |  |
| F2 no endless restart | `dgxtop-runtime/supervisor_hosts::unreaped_child_is_quarantined_and_never_respawned` |  |
| F3 at most 8 hosts | `dgxtop-runtime::supervisor::tests::start_rejects_too_many_or_duplicate_hosts`, `dgxtop-runtime::supervisor::config::tests::invalid_values_are_rejected` |  |
| F4 no thread per tick | `dgxtop-runtime/supervisor_hosts::threads_and_fds_do_not_grow_with_requests`, `dgxtop-runtime::queues::tests::threads_are_fixed_and_reused` |  |
| F5 a deadline does not abort the call | `dgxtop-runtime::supervisor::tests::timeout_reports_failure_and_discards_the_late_reply`, `dgxtop-runtime/supervisor_hosts::late_reply_is_discarded_and_host_recovers` |  |

<a id="at15"></a>

### AT15 — R15 Discovery, hot-plug and capability updates stay local

**Status: PASS.** Evidence: `gate-test`. Every clause has a passing test (fake sysfs/NVML backends).

| Clause | Tests | Note |
|---|---|---|
| S1 1000 hot-plug rounds | `dgxtop-adapters/adapters_a_catalog::thousand_hotplug_cycles_keep_the_catalog_bounded_and_never_reuse_slots`, `dgxtop-adapters/adapters_c_hwmon::hotplug_churn_keeps_identity_and_generations_monotonic` |  |
| S2 reconnect = new incarnation | `dgxtop-adapters/adapters_a_catalog::reconnected_device_is_a_new_incarnation_with_a_fresh_slot`, `dgxtop-adapters/adapters_a_network::recreated_interface_never_continues_the_old_counter` |  |
| S3 unchanged static metadata not re-read | `dgxtop-adapters/adapters_a_catalog::static_metadata_is_read_once_per_generation`, `dgxtop-adapters/adapters_b_nvml::metadata_is_cached_until_the_generation_changes` |  |
| F1 a half page never deletes the catalog | `dgxtop-adapters/adapters_a_catalog::half_page_discovery_never_deletes`, `dgxtop-manager::catalog::tests::pages_merge_and_removals_wait_for_the_complete_page` |  |
| F2 a vanished device affects only itself | `dgxtop-adapters/adapters_a_procfs::removed_disk_only_removes_its_own_sources`, `dgxtop-adapters/adapters_b_nvml::lost_gpu_only_affects_its_own_records` |  |
| F3 the source limit rejects, never OOM | `dgxtop-adapters/adapters_a_catalog::source_limit_keeps_host_wide_sources_and_reports_the_rest`, `dgxtop-manager::catalog::tests::sources_and_tombstones_are_bounded` |  |

<a id="at16"></a>

### AT16 — R16 Source diagnostics, age and visible exceptions

**Status: NOT_RUN.** Evidence: `gate-test`. The presentation and rendering tests below pass, but the owner removed the source diagnostics view from the interface for now (D-36), so a user cannot reach S1–S4. Restoring the tab restores the path.

| Clause | Tests | Note |
|---|---|---|
| S1 selected / excluded / alias / conflict reasons | `dgxtop-manager::presentation::sources::tests::candidates_show_dispositions_origin_and_evidence`, `dgxtop-manager::presentation::sources::tests::conflicting_and_quality_dispositions_are_named`, `dgxtop-ui::viewer::tests::sources_view_explains_arbitration` | Not reachable in the interface (D-36) |
| S2 age | `dgxtop-manager::presentation::sources::tests::candidates_show_dispositions_origin_and_evidence` |  |
| S3 effective interval | `dgxtop-ui::viewer::tests::sources_view_explains_arbitration`, `dgxtop-manager::scheduler::tests::effective_period_respects_minimum_multiplier_and_cap` |  |
| S4 policy revision | `dgxtop-ui::viewer::tests::sources_view_explains_arbitration` |  |
| S5 action states are clear | `dgxtop-ui::viewer::tests::actions_view_never_says_terminated`, `dgxtop-manager::process::tests::unknown_outcomes_from_recovery_are_surfaced_and_never_resent` |  |
| F1 unknown is never success-coloured or 0 | `dgxtop-ui::format::cell::tests::missing_states_render_their_labels_never_zero`, `dgxtop-ui::viewer::tests::missing_topics_render_no_data_instead_of_zero` |  |
| F2 a fixed value is only a suspect diagnostic | `dgxtop-collectors::arbitration::tests::an_unchanged_reading_is_only_a_suspect_diagnostic_never_fake_data` |  |
| F3 history gaps are visible | `dgxtop-ui::modules::history::strip::tests::columns_follow_time_and_keep_gaps_and_peaks`, `dgxtop-ui::widgets::sparkline::tests::gaps_are_blank_and_zero_is_lowest_bar`, `dgxtop-manager::presentation::history::tests::gaps_and_discontinuities_are_never_bridged` |  |

<a id="at17"></a>

### AT17 — R17 Observability, privacy and capacity targets

**Status: NOT_RUN.** Evidence: `gate-test`. S2 is not implemented: the diagnostics registry is never written out while monitoring, and nine OPERATIONS.md §3 metrics are never recorded (`source_age_ms` is not even defined). S3 has no test that scans the files written to disk.

| Clause | Tests | Note |
|---|---|---|
| S1 bounded labels | `dgxtop-core::diagnostics::tests::labels_are_bounded`, `dgxtop-manager::scheduler::tests::refused_dispatches_are_deferrals_labelled_by_reason` | Deferred dispatches carry one of three reasons (D-37) |
| S2 every queue/latency/quota/coverage metric output |  | NOT IMPLEMENTED (feature gap) |
| S3 cmdline and environment secrets never on disk | `dgxtop-store::audit::records::tests::intent_row_keeps_only_identity_uid_and_basename`, `dgxtop-core::text::tests::audit_command_names_drop_paths_but_keep_kernel_slashes` | PARTIAL: no code reads cmdline or environ; no disk scan |
| F1 100 000 short-lived PIDs stay bounded | `dgxtop-core::diagnostics::tests::labels_are_bounded`, `dgxtop-manager::results::tests::capacity_evicts_oldest_completed_but_never_pending`, `dgxtop-collectors::process::tests::bases_stay_bounded_under_heavy_process_churn` |  |
| F2 control characters escaped | `dgxtop-core::text::tests::audit_escapes_keep_hostile_bytes_visible_and_unambiguous`, `dgxtop-store::audit::records::tests::intent_rows_keep_an_escaped_command_name_with_its_slashes` | D-20 |
| F3 a full debug log counts drops | `dgxtop-runtime::logger::file::tests::pressure_drops_low_priority_first_and_counts_errors_separately` |  |

<a id="at18"></a>

### AT18 — R18 Staged migration and recoverable program/configuration

**Status: NOT_RUN.** Evidence: `gate-test`. F3 and F4 are covered for configuration and database files only; disk-full during install and an interrupted binary switch rely on the installer's structure (`set -e`, cleanup, final `mv -f`) without a test.

| Clause | Tests | Note |
|---|---|---|
| S1 a legacy dry-run maps everything | `dgxtop-store/store_persist_config::legacy_dry_run_maps_everything_and_writes_nothing`, `cli::legacy_import_dry_run_writes_nothing_and_apply_writes_private_v2_once` |  |
| S2 apply writes only v2 | `dgxtop-store/store_persist_config::legacy_apply_writes_v2_once_and_refuses_an_existing_v2` |  |
| S3 rollback starts the old version; v2 data kept | `cli::installer::installs_a_verified_release_keeps_the_previous_binary_and_rolls_back` | Stub old binary |
| F1 checksum corruption keeps a recoverable file | `cli::installer::a_checksum_mismatch_is_refused_and_the_installed_binary_is_kept`, `dgxtop-store/store_persist_audit::garbage_file_is_failed_and_preserved`, `dgxtop-store/store_persist_archive::corrupt_and_future_archives_are_kept_and_reported` |  |
| F2 unknown schema | `dgxtop-store/store_persist_config::future_schema_is_reported_and_never_rewritten`, `cli::future_schema_is_refused_and_never_rewritten` |  |
| F3 disk full | `dgxtop-store/store_persist_config::enospc_during_write_keeps_the_old_file` | PARTIAL: configuration write only |
| F4 an interrupted switch | `dgxtop-store/store_persist_config::rename_failure_keeps_the_old_file`, `dgxtop-store/store_persist_audit::failed_backup_aborts_the_migration_and_keeps_the_file` | PARTIAL: no binary-switch test |
| F5 rollback never claims to undo signals | `cli::installer::installs_a_verified_release_keeps_the_previous_binary_and_rolls_back` |  |

<a id="at19"></a>

### AT19 — R19 Existing rules kept, real verification records

**Status: NOT_RUN.** Evidence: `rules-preserved`, `spec-package-validation`. The executed parts pass. The test layer is a manual check plus the package lint, and the owner's manual check is not recorded.

| Clause | Tests | Note |
|---|---|---|
| S1 existing rules kept, no circular import |  | `rules-preserved`: the root `CLAUDE.md` is tracked and identical to HEAD; the spec's thin import is checked by `spec-package-validation` |
| S2 real commands and exit codes recorded |  | Every log here comes from `scripts/record_evidence.sh`; `scripts/check_validation.py` checks hashes and exit codes |
| F1 synthetic Python PASS never shown as Rust/hardware PASS |  | MANUAL |
| F2 the spec never relaxes user safety or commit authorization |  | MANUAL: no commit, tag or release was made |

<a id="at20"></a>

### AT20 — R20 Reproducible build, version and install integrity

**Status: BLOCKED.** Evidence: `gate-test`. Blocked on owner-authorized steps: Cargo.lock is untracked until committed, and the release workflow has never run.

| Clause | Tests | Note |
|---|---|---|
| S1 --version / manifest / tag agree | `cli::version_verbose_lists_build_metadata_and_sqlite_gate`, `cli::installer::a_binary_reporting_another_version_is_refused`, `cli::installer::a_requested_version_that_only_prefixes_the_binary_version_is_refused`, `contracts::structure::the_release_gate_matches_version_and_target_lines_exactly_and_checks_sqlite` | NEEDS_RELEASE for the tag check |
| S2 Cargo.lock tracked | `contracts::structure::the_lock_file_exists_is_not_ignored_and_the_toolchain_is_pinned_like_ci` | BLOCKED until committed |
| S3 --locked builds reproduce dependencies | `contracts::structure::workflows_pin_actions_by_commit_and_build_from_the_lock_file` | Static workflow check; NEEDS_RELEASE |
| S4 checksum verified | `cli::installer::installs_a_verified_release_keeps_the_previous_binary_and_rolls_back`, `cli::installer::a_checksum_mismatch_is_refused_and_the_installed_binary_is_kept`, `cli::installer::an_archive_without_a_checksum_entry_is_refused` |  |
| F1 a missing lock fails the build |  | NEEDS_RELEASE |
| F2 a version mismatch fails | `cli::installer::a_binary_reporting_another_version_is_refused`, `cli::installer::a_requested_version_that_only_prefixes_the_binary_version_is_refused` |  |
| F3 a wrong checksum fails | `cli::installer::a_checksum_mismatch_is_refused_and_the_installed_binary_is_kept` |  |
| F4 no automatic GNU → musl fallback | `cli::installer::musl_hosts_are_refused_without_a_fallback` |  |

<a id="at21"></a>

### AT21 — R21 Global budget and dual queue quotas

**Status: NOT_RUN.** Evidence: `gate-test`. F3 is not met for IPC decoding: arrays beyond their schema limits are decoded before the record and decoded-size checks, and credit beyond 3 × the wire size is reserved afterwards (bounded by the 65,536-value pre-scan cap).

| Clause | Tests | Note |
|---|---|---|
| S1 every allocation takes a BytePermit | `dgxtop-runtime::supervisor::tests::busy_while_in_flight_and_capacity_when_credit_is_missing`, `dgxtop-runtime::ipc::frame::tests::body_allocation_is_exactly_the_declared_length`, `dgxtop-store/store_hot_commit::at05_capacity_rejection_leaves_no_partial_state`, `dgxtop-store::history::accounted::tests::deque_capacity_is_reserved_before_growth`, `dgxtop-store/store_hot_query_limits::query_memory_is_reserved_up_front_and_released`, `dgxtop-manager::projection::tests::a_full_projection_budget_is_reserved_before_building_and_shrunk_after` | One test per site; "every" is confirmed by review |
| S2 32 items and 8 MiB together | `dgxtop-core::budget::tests::queue_gate_enforces_items_and_bytes_together` |  |
| S3 a 5 MiB batch fits | `dgxtop-core::budget::tests::reserve_release_and_never_overcommit`, `dgxtop-runtime::supervisor::credit::tests::whole_group_plans_reserve_the_bounded_worst_case` |  |
| F1 small batches can still exceed the byte cap | `dgxtop-core::budget::tests::queue_gate_enforces_items_and_bytes_together` |  |
| F2 release, hand-off and pinning never leak | `dgxtop-core::budget::tests::transfer_split_merge_keep_totals`, `dgxtop-core::budget::tests::random_reserve_release_balances`, `dgxtop-store::history::chunk::tests::evicted_chunk_stays_accounted_while_pinned` |  |
| F3 no allocate-then-check | `dgxtop-manager::projection::tests::a_ledger_without_room_for_a_full_projection_skips_the_frame_before_building`, `dgxtop-store::history::accounted::tests::failed_growth_leaves_deque_unchanged`, `dgxtop-runtime::ipc::frame::tests::oversize_prefix_is_rejected_without_allocating` | NOT MET for IPC decode |

<a id="at22"></a>

### AT22 — R22 Multi-rate scheduling, broker de-duplication, epochs

**Status: NOT_RUN.** Evidence: `gate-test`. S2's real-host check (identical reads cause one host request) is in the supervisor host suite, opt-in since D-53 and not run for this snapshot (user_opt_in_required); the other clauses pass.

| Clause | Tests | Note |
|---|---|---|
| S1 1 s / 5 s / 10 s / 60 s cadences | `dgxtop-manager::scheduler::tests::{cadences_follow_period_classes_and_discovery_interval, unsupported_adapter_is_never_read_and_probed_every_60_s}` |  |
| S2 one read per group and cycle | `dgxtop-runtime::broker::tests::identical_concurrent_reads_are_dispatched_once_and_share_the_result`, `dgxtop-runtime/supervisor_hosts::identical_reads_cause_one_host_request`, `dgxtop-manager::scheduler::tests::groups_of_one_adapter_share_its_phase_and_adapters_stay_spread` | D-29 |
| F1 a clock jump schedules only the next future slot | `dgxtop-manager::scheduler::tests::clock_jump_schedules_only_the_next_future_slot`, `dgxtop-core::clock::tests::next_due_skips_missed_slots_without_replay` |  |
| F2 old generation or epoch rejected | `dgxtop-manager::scheduler::tests::replies_of_old_epochs_and_generations_are_stale`, `dgxtop-manager::control::tests::at22_host_restart_drops_the_request_records_a_gap_and_rediscovers`, `dgxtop-runtime::broker::tests::different_cycle_generation_epoch_or_group_never_hit` |  |
| F3 no catch-up storm | `dgxtop-manager::scheduler::tests::clock_jump_schedules_only_the_next_future_slot` |  |

<a id="at23"></a>

### AT23 — R23 Fixed workers and a fair control loop

**Status: NOT_RUN.** Evidence: `gate-test`. S2's ≤ 2 ms control slice exists as a configuration range and a yield mechanism (tested with the fake clock), but no test asserts the bound itself.

| Clause | Tests | Note |
|---|---|---|
| S1 fixed worker count | `dgxtop-runtime::queues::tests::threads_are_fixed_and_reused` | Pool level |
| S2 control slice ≤ 2 ms | `dgxtop-manager::manager::tests::a_spent_slice_still_serves_one_item_per_category_and_yields`, `dgxtop-manager::manager::tests::a_fresh_slice_serves_at_most_the_category_batch` | PARTIAL: the bound is a config range without a test |
| S3 8 / 8 / 4 per loop, timers fair | `dgxtop-manager::control::tests::at23_iteration_is_fair_8_control_8_commands_4_ingestion` |  |
| F1 a command flood does not starve freshness | `dgxtop-manager::control::tests::at23_command_flood_is_busy_without_blocking_timers_and_shutdown_still_run`, `dgxtop-manager/control_thread::at23_flood_from_another_thread_is_busy_and_every_accepted_command_is_answered` |  |
| F2 an accepted command reserves a reply slot | `dgxtop-manager::session::tests::accepted_commands_reserve_slots_and_full_queue_is_busy` |  |
| F3 full → Busy | `dgxtop-manager::session::tests::outstanding_limit_rejects_even_with_queue_space` |  |
| F4 shutdown is never queued last | `dgxtop-manager::session::tests::shutdown_is_out_of_band_and_never_busy_on_full_queue` |  |

<a id="at24"></a>

### AT24 — R24 Blocked hosts and the quarantine limit

**Status: NOT_RUN.** Evidence: `gate-test`. F3 (TERM/KILL only for hosts this program created) rests on the launcher's own child handle without a negative test, and F2 counts descriptors only after teardown or for a healthy host.

| Clause | Tests | Note |
|---|---|---|
| S1 a stuck NVML host affects only its GPUs | `fault_injection::blocked_hosts::at24_hung_gpu_host_is_isolated_restarted_only_after_reap_and_leaks_nothing`, `fault_injection::blocked_hosts::at24_values_of_a_hung_backend_stop_being_shown_as_current`, `dgxtop-runtime/supervisor_hosts::other_host_keeps_serving_while_one_hangs` | Real replay child with hung groups; scaled timings |
| S2 a new generation only after reap | `dgxtop-runtime/supervisor_hosts::sigterm_ignoring_host_is_killed_with_sigkill` |  |
| F1 an unreapable child is not endlessly replaced | `fault_injection::blocked_hosts::at24_at39_unreapable_host_is_quarantined_never_respawned_and_shutdown_stays_bounded`, `dgxtop-runtime/supervisor_hosts::unreaped_child_is_quarantined_and_never_respawned` |  |
| F2 no descriptor leak | `dgxtop-runtime/supervisor_hosts::threads_and_fds_do_not_grow_with_requests` | PARTIAL |
| F3 TERM/KILL only self-created hosts | `contracts::structure::only_the_supervisor_launches_processes_and_nothing_runs_nvidia_smi` | STRUCTURAL |

<a id="at25"></a>

### AT25 — R25 Incremental projection and bounded snapshot lifetime

**Status: NOT_RUN.** Evidence: `gate-test`. F1 (two slow sessions never grow retired projections) is bounded only by the session cap and the latest-only slot, without a test; F2 is structural.

| Clause | Tests | Note |
|---|---|---|
| S1 projections only for visible topics | `dgxtop-manager::presentation::tests::each_view_fills_only_its_visible_topics`, `dgxtop-manager::presentation::tests::processes_view_copies_only_the_window` |  |
| S2 per session 1 pending + 1 active | `dgxtop-core::view::tests::slot_keeps_only_latest_and_releases_bytes`, `dgxtop-manager::projection::tests::published_projection_holds_its_bytes_until_dropped` |  |
| S3 a projection ≤ 1 MiB | `dgxtop-manager::presentation::tests::reference_load_projection_stays_under_one_mebibyte`, `dgxtop-manager::presentation::tests::small_budget_is_respected_and_announced`, `dgxtop-manager::projection::tests::oversized_or_unadmitted_projections_are_skipped` |  |
| F1 two slow sessions keep retired Arcs bounded |  | UNCOVERED |
| F2 a module cannot reach history or the store | `contracts::structure::implementation_crates_depend_on_no_workspace_crate_but_core` | PARTIAL / STRUCTURAL |
| F3 a cancelled history query drops the late view | `dgxtop-manager::history::tests::results_give_points_mean_peak_and_coverage_and_late_ones_are_dropped`, `dgxtop-manager::control::tests::history_view_queries_and_drops_stale_generations` |  |

<a id="at26"></a>

### AT26 — R26 Non-blocking terminal and backpressure

**Status: NOT_RUN.** Evidence: `gate-test`. Byte-level checks pass on a fake transport; the real-PTY checks of S1 and F1 (complete frames, a 30 s stalled reader does not block quit) are opt-in end-to-end tests since D-53 and were not run for this snapshot (user_opt_in_required).

| Clause | Tests | Note |
|---|---|---|
| S1 complete ANSI frames on a non-blocking PTY | `pty::replay_starts_shows_simulated_label_and_quits_with_q`, `dgxtop-ui::terminal::runner::tests::frames_never_interleave_under_partial_writes`, `dgxtop-runtime::terminal::tests::full_pipe_returns_zero_instead_of_blocking` |  |
| S2 a partial write restores the diff baseline | `dgxtop-ui::terminal::runner::tests::a_slow_frame_is_followed_by_a_full_redraw` |  |
| F1 reader stalled 30 s, q still works | `pty::stalled_reader_for_thirty_seconds_does_not_block_quit`, `dgxtop-ui::terminal::runner::tests::input_and_q_still_work_while_output_is_blocked_for_30_seconds` | Real PTY, real 30 s |
| F2 buffer ≤ 1 MiB | `dgxtop-ui::terminal::output::tests::an_oversized_frame_is_discarded_unsent_and_the_buffer_stays_bounded` |  |
| F3 no next frame before the flush | `dgxtop-ui::terminal::output::tests::frame_records_show_each_frame_started_after_the_previous_one_finished` |  |
| F4 no spliced half frames | `dgxtop-ui::terminal::runner::tests::frames_never_interleave_under_partial_writes` |  |

<a id="at27"></a>

### AT27 — R27 History window accuracy and cross-bucket counters

**Status: PASS.** Evidence: `gate-test`. Every clause has a passing test.

| Clause | Tests | Note |
|---|---|---|
| S1 59.5 → 60.5 s bridge kept | `dgxtop-store/store_hot_history::at27_cross_minute_interval_is_a_bridge_counted_exactly_once`, `dgxtop-store::history::rollup::tests::crossing_interval_becomes_a_bridge_exactly_once` |  |
| S2 exact totals counted once | `dgxtop-store/store_hot_properties::totals_of_disjoint_windows_add_up_to_their_union` |  |
| F1 no proportional guess at an evicted edge | `dgxtop-store/store_hot_history::at27_boundary_without_raw_is_a_boundary_gap_never_proportional` |  |
| F2 a missing segment shows boundary_gap | `dgxtop-store/store_hot_history::at27_boundary_without_raw_is_a_boundary_gap_never_proportional` |  |
| F3 2 < 10 < 100 ordering | `dgxtop-store/store_persist_archive::fixed_width_keys_sort_numerically`, `dgxtop-core::wire::tests::fixed_width_orders_lexically_like_numbers` |  |

<a id="at28"></a>

### AT28 — R28 REF performance and coverage thresholds

**Status: BLOCKED.** Evidence: `gate-test`, `bench-ref-smoke-60s`. Blocked: acceptance needs three 1800 s REF runs on a controlled host from a committed source with at least 24 h uptime for the 24 h history preload. Smoke runs are not acceptance evidence.

| Clause | Tests | Note |
|---|---|---|
| S1 3 × 1800 s, every metric within budget | `dgxtop::perf::tests::budgets_gate_runs_from_the_spec_file`, `dgxtop::perf::report::tests::gates_cover_limits_duration_and_sample_minimums`, `dgxtop-manager::control::tests::a_refused_read_dispatch_is_deferred_and_retried_never_counted_as_a_drop`, `dgxtop-manager::scheduler::tests::a_busy_worker_defers_the_due_read_until_the_retry_instant` | NEEDS_LONG_RUN. Smoke 3 × 60 s on the desktop with its GPUs idle and no other heavy process, right after the gates and hardware suites (load average 1.37 at start): CPU 4.550/4.667/4.767 % (<= 5), coverage 100.000/100.000/100.000 % (>= 99.9), frame p95 0.617/0.597/0.830 ms (<= 16), projection p95 51.994/51.887/51.987 ms (<= 100), RSS 212.484/236.598/250.336 MiB (<= 384). A refused dispatch is retried and is not an unexplained drop (D-37). |
| S2 sample minimums met | `dgxtop::perf::report::tests::gates_cover_limits_duration_and_sample_minimums` | Logic only |
| S3 raw evidence hashes verifiable | `dgxtop::perf::evidence::tests::files_are_fingerprinted_and_confined` |  |
| F1 kind = synthetic_example rejected | `dgxtop::perf::report::tests::only_a_complete_full_report_is_measured` | The spec evaluator's rejection tests ran in `spec-package-validation` |
| F2 missing proof rejected | `dgxtop::perf::report::tests::missing_metrics_unsampled_metrics_and_unknown_commits_block_measured` |  |
| F3 coverage < 99.9 fails | `dgxtop::perf::tests::budgets_gate_runs_from_the_spec_file`, `dgxtop::perf::coverage::tests::missing_reads_lower_coverage_and_extra_reads_never_raise_it`, `dgxtop::perf::coverage::tests::a_window_report_is_final_only_once_its_last_reads_could_commit` | D-33 |
| F4 throttle > 0 fails | `dgxtop::perf::report::tests::any_adaptive_throttle_event_fails_its_gate` |  |
| F5 never keep only the best run | `dgxtop::perf::report::tests::only_exactly_three_runs_of_the_reference_workload_can_be_measured` |  |

<a id="at29"></a>

### AT29 — R29 72-hour stability and source churn

**Status: BLOCKED.** Evidence: none. Blocked: two 72 h runs are not possible in this session, and the soak driver (72 h sampling of RSS, FDs and threads, plus churn generation) has not been written; the REF bench only imports soak results.

| Clause | Tests | Note |
|---|---|---|
| S1 fixed-source and churn soaks, 72 h each |  | NEEDS_LONG_RUN; soak driver missing |
| S2 RSS growth ≤ 16 MiB after 24 h |  | NEEDS_LONG_RUN |
| S3 FD and thread counts do not grow |  | Short-run analogue only |
| S4 zero crashes and deadlocks |  | NEEDS_LONG_RUN |
| F1 hot-plug, restarts and PID reuse stay bounded |  | Short-run analogues (AT15, AT09) |
| F2 a fake-clock 72 h is not a soak |  | MANUAL |

<a id="at30"></a>

### AT30 — R30 Overload state machine and observable degradation

**Status: NOT_RUN.** Evidence: `gate-test`. S1 and S2 pass on a fake clock; their checks with the real byte ledger in the fault-injection rig are opt-in end-to-end tests since D-53 and were not run for this snapshot (user_opt_in_required).

| Clause | Tests | Note |
|---|---|---|
| S1 queue ≥ 75 % for 3 samples → pressured | `dgxtop-manager::overload::tests::ingress_bytes_at_three_quarters_are_pressure_like_any_queue`, `dgxtop-manager::overload::tests::pressure_needs_consecutive_samples_and_three_seconds`, `fault_injection::overload::at30_ingress_bytes_at_75_percent_for_three_samples_enter_pressure` |  |
| S2 memory ≥ 90 % → throttled | `dgxtop-manager::overload::tests::memory_critical_throttles_then_escalates_and_recovers_one_level_per_30s`, `fault_injection::overload::at30_overload_escalates_recovers_one_level_per_30s_and_keeps_control_flowing` |  |
| S3 30 s healthy → down one level | `dgxtop-manager::overload::tests::unhealthy_sample_restarts_the_recovery_window` |  |
| S4 effective interval and gaps visible | `dgxtop-ui::viewer::tests::header_shows_the_host_and_warnings_and_the_footer_the_interval`, `dgxtop-manager::ingestion::tests::backlog_is_bounded_and_overflow_is_a_gap` |  |
| F1 no flapping | `dgxtop-manager::overload::tests::alternating_spikes_never_flap` |  |
| F2 reject, never OOM or retry forever | `dgxtop-manager::scheduler::tests::busy_host_records_skipped_due_and_never_sends_a_second_request`, `dgxtop-core::budget::tests::reserve_release_and_never_overcommit` |  |
| F3 control/audit never dropped for telemetry | `dgxtop-manager::events::tests::telemetry_is_evicted_before_control` |  |

<a id="at31"></a>

### AT31 — R31 SQLite version, WAL and disk limits

**Status: NOT_RUN.** Evidence: `gate-test`. F3's ENOSPC half has no test: nothing makes the audit intent write fail with SQLITE_FULL (the free-space check does not replace ENOSPC handling).

| Clause | Tests | Note |
|---|---|---|
| S1 linked SQLite ≥ 3.51.3 | `dgxtop-store/store_persist_sqlite::bundled_sqlite_passes_the_runtime_gate`, `dgxtop-store::sqlite::version::tests::floor_is_exactly_3_51_3` | Also `release-version-verbose` |
| S2 audit FULL, archive NORMAL | `dgxtop-store::audit::repository::tests::writer_connection_is_wal_full_with_manual_checkpoints`, `dgxtop-store::archive::sink::tests::writer_connection_is_wal_normal_with_manual_checkpoints` |  |
| S3 the writer runs its own checkpoints | `dgxtop-store/store_persist_audit::wal_backlog_pinned_by_a_reader_stops_new_intents_until_released` | Shown by behaviour |
| S4 total size queryable | `dgxtop-store/store_persist_audit::database_files_are_private` |  |
| F1 an old SQLite is refused | `dgxtop-store::audit::repository::tests::version_gate_failure_fails_closed_without_touching_disk`, `dgxtop-store/store_persist_sqlite::older_runtimes_and_mismatched_builds_are_refused` |  |
| F2 a stuck reader or a 16 MiB WAL refuses intents | `dgxtop-store/store_persist_audit::wal_backlog_pinned_by_a_reader_stops_new_intents_until_released`, `dgxtop-store::audit::limits::tests::defaults_match_the_storage_policy_table` |  |
| F3 low disk or ENOSPC: no signal | `action_disposable::at10::at10_audit_refusing_the_intent_sends_nothing`, `dgxtop-store/store_persist_audit::low_free_disk_refuses_new_intents_but_records_outcomes` | PARTIAL: low-disk half only |

<a id="at32"></a>

### AT32 — R32 Late audit results, cancellation, at-most-once action

**Status: PASS.** Evidence: `gate-test`. Every clause has a passing test; the end-to-end path also runs on a real disposable child.

| Clause | Tests | Note |
|---|---|---|
| S1 a token is used once | `dgxtop-manager::process::tests::ten_concurrent_confirms_of_one_token_send_exactly_once`, `action_disposable::at32::at32_token_is_single_use_across_sessions_concurrency_and_replays` |  |
| S2 send and cancel are linearized | `dgxtop-manager::process::tests::{cancel_before_send_wins_over_a_later_durable_intent, cancel_after_sending_is_too_late}` |  |
| S3 TTL and policy re-checked after the durable receipt | `dgxtop-manager::process::tests::{receipt_arriving_after_the_deadline_but_before_a_tick_does_not_send, policy_change_revokes_tokens_that_have_not_reached_sending}` |  |
| F1 late receipt / F2 cancel / F3 policy change | `dgxtop-manager::process::tests::{late_audit_receipt_after_the_intent_deadline_never_sends, cancel_before_send_wins_over_a_later_durable_intent, policy_change_revokes_tokens_that_have_not_reached_sending}` |  |
| F4 crash and restart: never resent | `dgxtop-store/store_persist_audit::recover_unknown_marks_unresolved_intents_and_never_returns_them_as_sendable`, `action_disposable::at32::at32_crash_after_a_durable_intent_is_unknown_after_restart_and_never_resent` |  |
| F5 unknown is never retried | `dgxtop-manager::process::tests::missing_send_report_is_unknown_and_never_retried` |  |
| F6 sent does not wait for exit | `action_disposable::at32::at32_signal_sent_is_reported_without_waiting_for_exit` |  |

<a id="at33"></a>

### AT33 — R33 IPC length, depth, generation and decode limits

**Status: NOT_RUN.** Evidence: `gate-test`. Framing, depth and record limits pass (property and exhaustive tests); the real-child checks of F1 and F4 (protocol violations restart the host, a wrong-nonce hello is rejected) are in the supervisor host suite, opt-in since D-53 and not run for this snapshot (user_opt_in_required).

| Clause | Tests | Note |
|---|---|---|
| S1 split length prefix and UTF-8 | `dgxtop-runtime::ipc::frame::tests::random_chunking_preserves_every_frame` |  |
| S2 schema and generation checked before commit | `dgxtop-runtime::ipc::decode::tests::{hello_identity_is_verified, unknown_message_types_are_rejected}`, `dgxtop-manager::scheduler::tests::replies_of_old_epochs_and_generations_are_stale` |  |
| S3 256 records, depth 32 | `dgxtop-runtime::ipc::decode::tests::{more_than_256_records_are_rejected, depth_limit_is_exact}` |  |
| F1 oversized prefix | `dgxtop-runtime::ipc::frame::tests::oversize_prefix_is_rejected_without_allocating`, `dgxtop-runtime/supervisor_hosts::protocol_violations_restart_the_host` |  |
| F2 too deep | `dgxtop-runtime::ipc::decode::tests::depth_limit_is_exact` |  |
| F3 EOF inside a frame | `dgxtop-runtime::ipc::frame::tests::eof_inside_prefix_or_body_is_an_error`, `dgxtop-runtime::host::tests::malformed_truncated_and_oversized_input_end_the_host` |  |
| F4 wrong host / generation / action operation | `dgxtop-runtime/supervisor_hosts::wrong_nonce_hello_is_rejected`, `dgxtop-runtime::host::tests::wrong_generation_or_adapter_is_a_protocol_violation` |  |
| F5 never allocate a declared huge size first | `dgxtop-runtime::ipc::frame::tests::any_declared_length_over_the_limit_never_allocates` |  |

<a id="at34"></a>

### AT34 — R34 Catalog cache and unsupported-probe throttling

**Status: PASS.** Evidence: `gate-test`. Every clause has a passing test.

| Clause | Tests | Note |
|---|---|---|
| S1 metadata cached until the generation changes | `dgxtop-adapters/adapters_b_nvml::metadata_is_cached_until_the_generation_changes`, `dgxtop-adapters/adapters_a_catalog::static_metadata_is_read_once_per_generation` |  |
| S2 unsupported probed at most once per 60 s | `dgxtop-adapters/adapters_b_nvml::unsupported_capability_is_reprobed_at_most_once_per_minute`, `dgxtop-adapters::common::tests::probe_throttle_limits_reprobes` |  |
| F1 one API error does not block forever | `dgxtop-adapters/adapters_b_nvml::transient_errors_are_retried_every_cycle`, `dgxtop-adapters/adapters_c_hwmon::reprobe_recovers_a_channel_after_the_interval` |  |
| F2 a driver restart clears the cache | `dgxtop-adapters/adapters_b_nvml::driver_reload_starts_a_new_generation` |  |
| F3 no resident nvidia-smi | `contracts::structure::only_the_supervisor_launches_processes_and_nothing_runs_nvidia_smi` |  |

<a id="at35"></a>

### AT35 — R35 Replayable fixtures and benchmark evidence

**Status: NOT_RUN.** Evidence: `gate-test`, `spec-package-validation`. S3 is only partly met: the fixture regenerates byte-identically from its seed and every evidence file is fingerprinted, but nothing re-derives the reported metrics from the raw files.

| Clause | Tests | Note |
|---|---|---|
| S1 same seed → identical bytes | `dgxtop-adapters/adapters_c_replay::same_seed_produces_identical_bytes_and_other_seeds_differ`, `dgxtop::perf::fixture::tests::same_seed_gives_byte_identical_fixtures` |  |
| S2 every metric type, separate namespace | `dgxtop::perf::fixture::tests::reference_fixture_contains_every_metric_class_in_the_replay_namespace`, `replay::replay_values_are_never_live_or_history_eligible` |  |
| S3 raw evidence can be re-run | `dgxtop::perf::fixture::tests::written_fixture_matches_its_fingerprint`, `dgxtop::perf::evidence::tests::files_are_fingerprinted_and_confined` | PARTIAL |
| F1 synthetic input never poses as measured | `dgxtop-adapters/adapters_c_replay::validation_rejects_live_sample_kinds`, `dgxtop::perf::report::tests::only_a_complete_full_report_is_measured` |  |
| F2 fewer hosts/series or a wrong config rejected | `dgxtop::perf::report::tests::only_exactly_three_runs_of_the_reference_workload_can_be_measured` |  |

<a id="at36"></a>

### AT36 — R36 Uncertain outcomes of versioned configuration persistence

**Status: PASS.** Evidence: `gate-test`. Every clause has a passing test; the step order is shown by the failpoints together.

| Clause | Tests | Note |
|---|---|---|
| S1 atomic write, rename, directory fsync | `dgxtop-store/store_persist_config::{cas_success_increments_revision_with_private_modes, a_failure_before_the_rename_keeps_the_old_file, directory_fsync_failure_after_rename_is_uncertain_with_the_new_content}` |  |
| S2 the receipt matches a valid epoch | `dgxtop-manager::control::tests::at22_at08_late_reply_of_the_old_epoch_is_dropped_after_a_config_change` |  |
| S3 a relaunch loads the full disk version | `dgxtop-store/store_persist_config::crash_after_persist_before_activation_reloads_the_new_version` |  |
| F1 directory fsync fails → commit_uncertain and reload | `dgxtop-store/store_persist_config::directory_fsync_failure_after_rename_is_uncertain_with_the_new_content`, `dgxtop-manager::config::tests::uncertain_commit_adopts_the_on_disk_configuration` | D-25 |
| F2 no concurrent overwrite | `dgxtop-store/store_persist_config::{two_repositories_cannot_both_commit_the_same_revision, external_concurrent_modification_conflicts}` |  |

<a id="at37"></a>

### AT37 — R37 Release gate separate from hardware evidence

**Status: BLOCKED.** Evidence: `gate-test`. Blocked: the CI and release workflows have never run (no commit or tag was authorized), and there is no aarch64 / DGX Spark evidence.

| Clause | Tests | Note |
|---|---|---|
| S1 normal CI and hardware gates separate |  | All `hardware_*` tests are `#[ignore]`; `just hardware` runs them; NEEDS_RELEASE for a CI run |
| S2 validated only for GNU platforms with evidence |  | This report claims no validated platform |
| F1 missing hardware or soak report → BLOCKED | `dgxtop::perf::report::tests::only_a_complete_full_report_is_measured` |  |
| F2 a cross-compile PASS is not GPU support |  | MANUAL; NEEDS_HARDWARE |
| F3 a source version mismatch is refused | `cli::installer::a_binary_reporting_another_version_is_refused`, `dgxtop::perf::environment::tests::commits_are_reported_only_when_known`, `dgxtop::provenance::tests::unstaged_edits_and_branch_moves_re_run_the_build_script` | D-34 |

<a id="at38"></a>

### AT38 — R38 Bounded cardinality, query and NSS cost

**Status: PASS.** Evidence: `gate-test`. Every clause has a passing test; scan and byte limits run at scaled values with the defaults asserted separately.

| Clause | Tests | Note |
|---|---|---|
| S1 ≤ 2000 points, 200 k scanned, 4 MiB | `dgxtop-store/store_hot_query_limits::invalid_queries_are_validation_failures`, `dgxtop-store::query::tests::point_cap_respects_request_config_and_bytes`, `dgxtop-core::config::tests::query_and_worker_defaults_match_the_runtime_limits` |  |
| S2 2 workers, 4 jobs | `dgxtop-manager::history::tests::every_chart_queries_one_window_at_a_bounded_resolution`, `dgxtop-runtime::queues::tests::busy_when_the_queue_is_full_and_never_blocks`, `dgxtop-store/store_hot_query_limits::at38_two_history_workers_query_while_commits_continue` |  |
| S3 UID lookup off the control/UI threads, numeric fallback | `dgxtop-manager::presentation::processes::tests::rows_carry_gpu_labels_numeric_users_and_sanitized_names` | No NSS lookup exists |
| F1 a pathological query → QueryTooLarge or Busy | `dgxtop-store/store_hot_query_limits::at38_scan_budget_returns_query_too_large_with_a_coarser_suggestion`, `dgxtop-store/store_hot_query_limits::query_memory_is_reserved_up_front_and_released` |  |
| F2 a stuck NSS cannot freeze the screen | `contracts::structure::core_and_the_viewer_carry_no_system_or_driver_dependencies` | No NSS call exists |
| F3 a long reader never pins all memory | `dgxtop-store::history::pin::tests::chunks_pinned_by_a_running_query_stay_accounted_after_eviction`, `dgxtop-store/store_hot_query_limits::at38_deadline_returns_timed_out` |  |

<a id="at39"></a>

### AT39 — R39 Layered errors and safe shutdown

**Status: NOT_RUN.** Evidence: `gate-test`. S2, S4 and F1 to F3 also rest on fault-injection and real-child tests, opt-in end-to-end suites since D-53 that were not run for this snapshot (user_opt_in_required); F1 has no other test. Real-time shutdown checks allow 0.5–0.8 s of scheduling slack over the fake-clock 3 s bound.

| Clause | Tests | Note |
|---|---|---|
| S1 a read failure stays local | `dgxtop-adapters/adapters_a_procfs::offline_cpu_only_affects_itself`, `dgxtop-runtime::host::tests::adapter_panic_sends_a_failed_reply_and_exits`, `dgxtop-manager::ingestion::tests::failures_and_timeouts_free_the_collector` |  |
| S2 a Manager invariant error → fail-stop | `fault_injection::safe_shutdown::at39_manager_invariant_failure_fails_stop_and_never_sends`, `dgxtop::fail_stop::tests::a_panic_halts_requests_shutdown_and_keeps_the_first_reason`, `dgxtop::control_ops::tests::a_fail_stop_refuses_every_later_send` | D-19 |
| S3 terminal restore attempted within 250 ms | `dgxtop-ui::terminal::runner::tests::input_and_q_still_work_while_output_is_blocked_for_30_seconds`, `dgxtop-ui::terminal::output::tests::the_restore_tail_always_fits_after_a_full_frame` |  |
| S4 control shutdown ≤ 3 s | `dgxtop-manager::control::tests::at39_shutdown_waits_at_most_three_seconds_and_reports_unfinished_work`, `fault_injection::safe_shutdown::at39_shutdown_with_hung_config_and_audit_storage_is_bounded_and_honest`, `dgxtop::monitor::tests::shutdown_waits_share_one_deadline` | D-24 |
| F1 an unreapable host: warn, never wait forever | `dgxtop-runtime/supervisor_hosts::shutdown_is_bounded_with_stubborn_hosts`, `fault_injection::blocked_hosts::at24_at39_unreapable_host_is_quarantined_never_respawned_and_shutdown_stays_bounded` |  |
| F2 audit unknown or state error → no signal | `fault_injection::store_partitions::at06_at39_stuck_audit_times_out_the_intent_and_its_late_receipt_never_sends`, `action_disposable::at32::at32_crash_after_a_durable_intent_is_unknown_after_restart_and_never_resent` |  |
| F3 stuck disk output never locks the loop | `fault_injection::store_partitions::at06_stuck_then_full_archive_never_stalls_hot_store_commits`, `dgxtop-runtime::logger::file::tests::pressure_drops_low_priority_first_and_counts_errors_separately` |  |

<a id="at40"></a>

### AT40 — R40 Concrete performance evidence and full specification validation

**Status: NOT_RUN.** Evidence: `spec-package-validation`, `spec-copy-verification`, `gate-scripts`. Not complete: the spec package's browser-based HTML checks need Chromium and a Python package that are not installed here.

| Clause | Tests | Note |
|---|---|---|
| S1 all 40 items have a task/AT/evidence status | `scripts/test_check_validation.py::CheckValidationTests::test_missing_duplicate_and_orphan_ids_are_rejected` | This report; `scripts/check_validation.py` enforces it in CI |
| S2 ZIP CRC, hashes, links, schema and HTML complete | `scripts/test_verify_spec_copy.py::VerifySpecCopyTests::test_an_identical_copy_passes` | Browser HTML checks NOT_RUN |
| F1 missing/empty file, orphan id, wrong hash or untested PASS refused | `scripts/test_check_validation.py::CheckValidationTests::test_wrong_hash_wrong_exit_and_missing_logs_are_rejected`, `scripts/test_check_validation.py::CheckValidationTests::test_pass_needs_every_cited_test_to_have_passed` |  |
| F2 spec and software acceptance kept apart |  | This report keeps them apart |

## Benchmarks (smoke, never acceptance)

Every run below is a smoke run. Each report carries `kind: synthetic_example` and lists its
blockers, so none of it is acceptance evidence.

The REF workload:

- seven replay hosts with 1 ms of simulated API latency;
- 2048 sources;
- a headless 160×50 terminal;
- 1 s sampling and 4 fps rendering;
- a 10 Hz script of view commands.

Acceptance still requires three 1800 s runs on a controlled host, from a committed source
(PERFORMANCE.md §2).

The recorded runs ran on the desktop with its GPUs idle and no other heavy process, right after the gates and hardware suites (load average 1.37 at start) and pass every budget. The CPU margin
depends on the machine's load:

- On 2026-10-05, with two other processes at 100 % CPU (load average 2 to 3.7), 60 s runs
  measured up to 5.217 % CPU in a single run.
- An A/B against the tree before the overview redesign found the extra CPU only in code the
  redesign did not change (process README, REF CPU work).
- One run on that day counted a refused adapter dispatch as an unexplained drop at 100 %
  coverage. The dispatch had been retried. Since D-37 such refusals are counted as
  deferrals, and the run details list them by reason.
- On 2026-10-08, while the owner's GPU job ran (one process at 100 % CPU), two recordings
  of the D-41 tree measured 4.483 / 4.500 / 4.750 % and 4.833 / 4.783 / 4.883 %: a
  0.4-point spread from load alone. The idle recordings of D-43 to D-46 matched the idle
  D-40 recording (4.533 / 4.533 / 4.600 %).
- Since D-48 the history view also queries each GPU's 1 h to 24 h statistics. With the REF
  fixture's eight GPUs a minute runs 1704 history queries instead of 168, at a lower history
  p95 (a whole-window query is cheap). The D-48 recording measured 4.550 / 4.533 / 4.817 %:
  within budget, the third 0.18 points below it, recorded right after the gates. No A/B
  under equal load against D-46 was run (NOT_RUN).
- D-49 changes only what the GPU view draws and drops the processes from its projection.
  The D-49 recording measured 4.633 / 4.783 / 4.850 % while another agent's Python process
  used 14 to 16 % of one core. No A/B under equal load against D-48 was run (NOT_RUN).
- D-50 charts four series per GPU in the history view: 38 charts instead of 14 with the REF
  fixture's eight GPUs. A first run, beside a second dgxtop of a live check, measured
  4.600 / 5.133 / 4.717 %: one run over budget. One cost was found: at 160×50 a history plot
  has 53 columns while the HistoryService queries at least 60 points, so every projection
  copied and downsampled every chart. The history view now keeps the queried points and the
  Viewer merges them into its columns; the next run measured 4.767 / 4.717 / 4.633 %.
  The recorded runs measured 4.550 / 4.667 / 4.767 %. No A/B under equal load against D-49 was run
  (NOT_RUN).

| Bench | Metric | Budget | Measured per run | Result |
|---|---|---|---|---|
| REF, 3 × 60 s | `adaptive_throttle_events` | <= 0 | 0.000 / 0.000 / 0.000 | within budget |
| REF, 3 × 60 s | `command_p95_ms` | <= 50 | 0.611 / 0.583 / 0.617 | within budget |
| REF, 3 × 60 s | `command_p99_ms` | <= 100 | 0.633 / 0.618 / 0.651 | within budget |
| REF, 3 × 60 s | `coverage_percent` | >= 99.9 | 100.000 / 100.000 / 100.000 | within budget |
| REF, 3 × 60 s | `cpu_mean_one_core_percent` | <= 5 | 4.550 / 4.667 / 4.767 | within budget |
| REF, 3 × 60 s | `crashes` | <= 0 | 0.000 / 0.000 / 0.000 | within budget |
| REF, 3 × 60 s | `deadlocks` | <= 0 | 0.000 / 0.000 / 0.000 | within budget |
| REF, 3 × 60 s | `frame_p95_ms` | <= 16 | 0.617 / 0.597 / 0.830 | within budget |
| REF, 3 × 60 s | `frame_p99_ms` | <= 33 | 0.781 / 0.775 / 0.886 | within budget |
| REF, 3 × 60 s | `history_p95_ms` | <= 100 | 0.020 / 0.030 / 0.028 | within budget |
| REF, 3 × 60 s | `managed_peak_mib` | <= 256 | 31.095 / 31.127 / 31.127 | within budget |
| REF, 3 × 60 s | `projection_p95_ms` | <= 100 | 51.994 / 51.887 / 51.987 | within budget |
| REF, 3 × 60 s | `rss_tree_peak_mib` | <= 384 | 212.484 / 236.598 / 250.336 | within budget |
| REF, 3 × 60 s | `unexplained_drops` | <= 0 | 0.000 / 0.000 / 0.000 | within budget |
| projection | `frame_p95_ms` | <= 16 | 0.371 / 0.450 / 0.484 | within budget |
| projection | `frame_p99_ms` | <= 33 | 0.434 / 0.480 / 0.489 | within budget |
| history | `history_p95_ms` | <= 100 | 0.123 / 0.127 / 0.123 | within budget |

The minimum duration and sample-count gates fail in every smoke run by design and are
left out of the table; the evidence logs list them.

Micro benchmarks that measure no budgeted metric report a note instead:

- arbitration (`bench-arbitration-smoke`): worst per-cycle collect p95 8.425 ms (component of projection_p95_ms, budget 100 ms).

## Known gaps and next steps

1. **AT17 S2, missing feature.** The diagnostics registry is never written out while
   monitoring. Nine OPERATIONS.md §3 metrics are never recorded, and `source_age_ms` is not
   defined.
2. **AT21 F3.** IPC decoding builds arrays beyond their schema limits before the record and
   decoded-size checks. The fix:
   - enforce the record and reason-code limits while deserializing;
   - reserve decode credit from the structural pre-scan before serde runs;
   - refuse a batch whose decoded size exceeds that bound, instead of growing the credit
     afterwards.
3. **Port exclusivity is checked only by types.** This covers AT01 S1/S3, AT05 S3/F3, AT24 F3
   and AT25 F2. Name scans in `tests/contracts/structure.rs` would close it, each with a
   negative self-test.
4. **Missing tests:**
   - AT31 F3: a real SQLITE_FULL on the audit intent write;
   - AT07 F3: counters across a UTC step;
   - AT13 S1: a held projection stays unchanged;
   - AT23 S2: the 2 ms slice bound;
   - AT25 F1: two slow sessions;
   - AT18 F3/F4: installer disk-full and an interrupted switch.
5. **Owner decisions (D-55):**
   - The Sources and Processes tabs stay hidden (D-36), which leaves AT12 S1 and AT16 without
     that user path.
   - NVML PCIe throughput stays the driver's "KB/s" reading; the canonical value is unavailable
     (D-11). NVML's 32-bit byte counters wrap too often to replace it.
6. **DGX Spark:**
   - The GPU share of RAM on unified memory adds up only the processes the driver lists. On
     the first DGX run, compare it with `nvidia-smi` and `free -h`.
   - The GB10 hwmon power mapping (D-10) is `usable` until checked on real hardware.
7. **Blocked work:**
   - AT28 needs three 1800 s REF runs from a committed source with ≥ 24 h uptime.
   - AT29 needs a soak driver and two 72 h runs.
   - AT20 and AT37 need a commit, a tag and a CI/release run.
8. **Specification tooling:**
   - The spec package's browser HTML checks were not run (Chromium and its Python package
     are not installed).
   - The spec's Python tests ran before this session's changes, and the spec copy has not
     changed since (`spec-copy-verification`). Re-running them needs the pinned `jsonschema`,
     which is not installed.
9. **Opt-in runs (D-53):**
   - The end-to-end suites (`just e2e`, recorded as `e2e-suites`) run only on the owner's
     request. A passing recording returns AT06, AT11, AT14, AT22, AT26, AT30, AT33 and AT39 to
     PASS.
   - The hardware suites and benchmarks were last recorded on the D-50 tree. A REF A/B with
     the history archive on (the D-51 default) against off has not been run.
