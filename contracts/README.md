# dgxtop contracts

The JSON Schemas here are dgxtop's own contracts: the sealed spec's schemas
(`spec/dgxtop-v2/contracts/`) plus the fields dgxtop's decisions add. They are
built by `scripts/build_contracts.py`, never edited by hand, and the contract
tests hold every document dgxtop writes to them. Every document the spec
accepts is still accepted.

| Schema | Added | Decision |
|---|---|---|
| `command.schema.json` | `UpdateViewSession.overview_bar`, `UpdateViewSession.process_bar`: the bar sizes of the Overview History and the Process History (`auto`, `1m` … `24h`), optional; the history view's `filter` stays empty | D-77, D-78, D-80 |
| `command.schema.json`, `config.schema.json` | `store.process_history_enabled`: whether the Process History is kept on disk, optional, unset following `store.archive_enabled` | D-80 |
| `command.schema.json`, `config.schema.json` | Retired, not added: `store.raw_candidate_seconds` is no longer required. A document that still has it stays valid (the spec's range) and dgxtop ignores it; dgxtop never writes it | D-88 |
| `command.schema.json`, `config.schema.json` | Retired, not added: `store.max_query_scan_records` and `runtime.max_history_result_bytes` are no longer required. A document that still has them stays valid (the spec's ranges) and dgxtop ignores them; dgxtop never writes them | D-90 |

Schemas dgxtop does not extend are used from the spec as they are, except
`history-query.schema.json` and `history-result.schema.json`: the query engine
they describe is removed (D-90), so dgxtop reads and writes no document of
them.
