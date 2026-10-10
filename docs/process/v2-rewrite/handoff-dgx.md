# Handoff: continue on the DGX Spark

Written 2026-10-10 on the owner's desktop, before the work moves to a DGX Spark (GB10). The
desktop session's notes about the owner live outside the repository, so the rules they hold are
copied here. Start with [README.md](README.md), [DECISIONS.md](DECISIONS.md) and
[VALIDATION.md](../../validation/VALIDATION.md).

## Where the work stands

- The v2 rewrite is committed on branch `v2-rewrite` and pushed. It is not merged into `main`.
- [spec/dgxtop-v2/](../../../spec/dgxtop-v2/) is the specification. It must stay byte-identical
  to `dgxtop-spec-v2.zip`:

  ```bash
  python3 scripts/verify_spec_copy.py dgxtop-spec-v2.zip spec/dgxtop-v2
  ```

- Decisions D-01 to D-55 record every refinement of the specification. The latest:
  - D-51: the history archive is on by default.
  - D-52: bars and charts never touch.
  - D-53: fewer tests; the end-to-end suites run only on request.
  - D-54: topology, instant power, fans and the `TYPE` column.
  - D-55: the PCIe unit stays the driver's reading; the Sources tab stays removed.
- Validation: PASS 12, FAIL 0, NOT_RUN 24, BLOCKED 4 (see VALIDATION.md).
- Last verified on the desktop, on the tree of this commit:
  - Machine: x86_64, i9-13900K, RTX 5080 + RTX 3060, driver 595.104.02.
  - Passed: fmt, check, clippy `-D warnings` and test (1358 passed, 0 failed, 42 opt-in
    ignored), all `--workspace --all-features --locked`.
  - Also passed: dependency boundaries, script tests, the spec copy check, and the release
    build.
  - `Cargo.lock` SHA-256 is `ebe26984a218038f7892b8eb12d3b607593e86d4460c07da1ce0276f5c1cea74`.
- Not run for this tree:
  - the end-to-end suites (`just e2e`);
  - the hardware suites (`just hardware`);
  - the benchmarks (their logs are from the D-50 tree);
  - the REF A/B with the archive on and off.
- dgxtop has never been built or run on aarch64. The first build there is evidence in itself.

## What to check on the DGX Spark

Run only what the owner asks for in the current request. The items waiting for this machine
are:

1. **Build:**

   ```bash
   cargo build --release --locked
   ```

   `aarch64-unknown-linux-gnu` is a specification target. Then run the core gates with `just ci`.
2. **Unified memory:** the GPU's `RAM` bar and the RAM GPU processes hold. Compare them with
   `nvidia-smi` and `free -h`. Today the figure adds up only the processes the driver lists.
3. **GB10 hwmon power mapping (D-10):** compare the CPU power readings with the platform's
   sensors. It stays `usable` until checked on real hardware.
4. **Hardware suites:**

   ```bash
   cargo test --workspace --all-features --locked -- --ignored hardware_ --test-threads=1
   ```

5. **The D-54 rows on GB10:**
   - Topology: a lone GPU has no peer rows.
   - Instant power and fans: hidden when NVML does not report them.
   - `TYPE` in the process table.
6. **GPM, only if the owner wants SM, tensor and DRAM activity (D-54 b).** The RTX 5080 has it
   and the RTX 3060 does not. Probe read-only first:

   ```bash
   nvidia-smi dmon --gpm-metrics 1,2,3,10 -c 3
   ```

   It needs a sampler that keeps one GPM sample between reads, which is a design change.

## Recording evidence

`scripts/record_evidence.sh <name> <command...>` writes a self-describing log to
`docs/validation/evidence/<name>.log` with the host, commit, timestamps and exit code. GPU
UUIDs and serials are redacted.

For DGX runs, use new names such as `hardware-suites-gb10`, so the desktop's x86_64 evidence is
kept. Add the new row to the "Evidence logs" table in VALIDATION.md (name, command, exit code,
SHA-256).

`python3 scripts/check_validation.py` checks every hash and every test that a PASS row cites.
It names any row that no longer matches its log.

The desktop's default recording order was:

```bash
scripts/record_evidence.sh gate-fmt cargo fmt --all -- --check
scripts/record_evidence.sh gate-check cargo check --workspace --all-targets --all-features --locked
scripts/record_evidence.sh gate-clippy cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
scripts/record_evidence.sh gate-test cargo test --workspace --all-features --locked --no-fail-fast
scripts/record_evidence.sh release-build cargo build --release --locked
scripts/record_evidence.sh release-version-verbose target/release/dgxtop --version --verbose
```

The end-to-end suites use the name `e2e-suites`:

```bash
cargo test --workspace --all-features --locked --no-fail-fast -- --ignored --skip hardware_
```

## How the owner works

- **Language.** Reply in Traditional Chinese. The UI, code comments and docs are in English.
- **Authority.**
  - No commit, push, tag or deploy without the owner's explicit request in that conversation.
  - Do not launch agents unless asked.
  - Do not install tools or dependencies.
  - Build with `--locked` and keep `Cargo.lock` unchanged.
- **Validation (`core-fast60`).**
  - The default `cargo test` is the core gate.
  - End-to-end, PTY, visual, hardware and multi-platform checks run only when the owner asks in
    the current request. Otherwise report them NOT_RUN (`user_opt_in_required`).
  - A validation batch is bounded to about 60 s.
  - Do not rerun without changed inputs or a named failure.
  - Missing hardware is BLOCKED. Never lower a threshold, and never claim a PASS without
    executed evidence.
- **Engineering.**
  - A bug fix starts with a failing test.
  - Follow SOLID and the CLAUDE.md commit rules. `CLAUDE.md` must stay identical to `HEAD`.
- **The owner's processes.**
  - Never signal them; tests use disposable children only.
  - Never persist command lines, environments or tokens.
  - The owner may run their own dgxtop and GPU jobs on the same machine. Run live tests with
    isolated `XDG_CONFIG_HOME` and `XDG_STATE_HOME` under `target/ui-check/xdg/`, given as a
    physical path; the store refuses symlinked paths.
  - On the desktop a supervisor restarted the owner's dgxtop from `target/release/dgxtop` on
    `q`, so build release only after the gates pass and tell the owner to press `q`. Ask
    whether the DGX has the same setup.
- **Taste in charts and views.**
  - Block charts with a scale and a now/avg/max legend.
  - Bars are `▆` (three quarters of a row) and charts leave headroom, so nothing touches at full
    load.
  - No italics: tmux shows italics as reverse video. Missing values are dim only.
  - No notes in the GPU view, and no rows for values the GPU does not report.
  - The GPU view is four live bars over `key value` blocks.
  - The history view is a dashboard and is full at 213×41 with two GPUs. The owner declined a
    clock panel there.
  - The "throttled for" totals stay as they are, and the Sources tab stays removed.
  - Check layouts at the owner's terminal size: 213×41 on the desktop. Ask on the DGX.

## Facts learned on the desktop GPUs

These come from read-only NVML probes, not from the test suites. Recheck them on GB10.

- NVML's `power_usage` equals the one-second average field (`NVML_FI_DEV_POWER_AVERAGE`). The
  instantaneous field is separate.
- The RTX 3060's `power` violation counter grows at 100 % while the card idles; the RTX 5080's
  does not. This is why no throttle share is derived (D-54 a).
- The PCIe byte counters (`NVML_FI_DEV_PCIE_COUNT_TX_BYTES`/`RX_BYTES`) are 32-bit and wrap every
  4 GiB (D-55).
- The two cards have no P2P (path `PHB`), and neither reports memory temperature.

## Open work

See VALIDATION.md "Known gaps" and [README.md](README.md) "Next action":

- the metrics output (AT17);
- the IPC decode credit (AT21);
- port exclusivity;
- the listed missing tests and the soak driver;
- then the long REF and soak runs.

An optional optimization: after the archive's WAL first reaches its 16 MiB limit, a passive
checkpoint runs at every archive write. This is correct, but more often than designed.
