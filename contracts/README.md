# dgxtop contracts

The JSON Schemas here are the documents dgxtop reads and writes: commands and their results,
the configuration, adapter manifests, observations and observation batches over the adapter IPC,
canonical samples, display modules and the benchmark report. The contract tests
(`tests/contracts`) hold every document dgxtop writes to them, parse every example in
`examples/` into its Rust type and write it back schema-valid, and check that the closed enum
sets of the schemas equal the Rust enums. Change a schema together with the Rust type it
describes.

| Schema | What dgxtop adds or retires | Decision |
|---|---|---|
| `command.schema.json` | `UpdateViewSession.overview_bar`, `UpdateViewSession.process_bar`: the bar sizes of the Overview History and the Process History (`auto`, `1m` … `24h`), optional; the history view's `filter` stays empty | D-77, D-78, D-80 |
| `command.schema.json`, `config.schema.json` | `store.process_history_enabled`: whether the Process History is kept on disk, optional, unset following `store.archive_enabled` | D-80 |
| `command.schema.json`, `config.schema.json` | Retired: `store.raw_candidate_seconds` is no longer required. A document that still has it stays valid and dgxtop ignores it; dgxtop never writes it | D-88 |
| `command.schema.json`, `config.schema.json` | Retired: `store.max_query_scan_records` and `runtime.max_history_result_bytes` are no longer required. A document that still has them stays valid and dgxtop ignores them; dgxtop never writes them | D-90 |
| `command.schema.json`, `config.schema.json` | Retired: `viewer.redline_threshold_percent` is no longer required. Level colors run from green at 0 % to red at 100 % (D-104), so no fill turns them red early. A document that still has it stays valid and dgxtop ignores it; dgxtop never writes it | D-105 |
| `command.schema.json` | `DeleteRuns`: deletes ended runs from the Process History, 1 to 256 process keys a command; a run still running stays | D-108 |
| `command.schema.json`, `config.schema.json` | `SetFolded` and `viewer.folded`: the panels folded away (the Overview's `disk` and `network`, the Overview History's `history.*` panels), saved without an expected revision; `viewer.folded` is optional, absent meaning none, and dgxtop writes it only when a panel is folded | D-114 |

`examples/config.reference.json` is the configuration of the REF workload, the reference
benchmark. dgxtop's built-in defaults differ from it: the history archive is on (D-51) and
SIGKILL is allowed (D-71). It still carries the keys D-88, D-90 and D-105 retired, as an
earlier version wrote them.
