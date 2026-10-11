#!/usr/bin/env python3
"""Builds dgxtop's contract schemas in `contracts/` from the sealed spec's.

The spec (`spec/dgxtop-v2/contracts/`) is sealed and byte-checked, so the
fields dgxtop adds by its decisions are written here instead, as the spec's
schema plus exactly these extensions (D-80):

* `UpdateViewSession.overview_bar` and `.process_bar`: the bar sizes of the
  Overview and the Process History (D-77, D-78), optional;
* `store.process_history_enabled`: whether the Process History is kept on
  disk, optional (unset follows `store.archive_enabled`), wherever the spec
  has the store settings (the config, and the config a command carries);
* `store.raw_candidate_seconds` is retired (D-88), and
  `store.max_query_scan_records` and `runtime.max_history_result_bytes`
  with the query engine they bounded (D-90): no longer required, still
  accepted with the spec's range, and never written by dgxtop.

Every document the spec accepts stays accepted. `--check` fails when the
committed files differ from what this script builds.
"""
import argparse
import copy
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SPEC = ROOT / 'spec' / 'dgxtop-v2' / 'contracts'
OUT = ROOT / 'contracts'
BAR_SIZES = ['auto', '1m', '5m', '10m', '30m', '1h', '2h', '4h', '6h', '8h', '12h', '18h', '24h']
BAR = {'type': 'string', 'enum': BAR_SIZES}
# Keys an earlier version wrote and dgxtop ignores, by section (D-88, D-90).
RETIRED_STORE_KEYS = ['raw_candidate_seconds', 'max_query_scan_records']
RETIRED_RUNTIME_KEYS = ['max_history_result_bytes']


def objects(node):
    """Every object schema in `node`, depth first."""
    if isinstance(node, dict):
        yield node
        for value in node.values():
            yield from objects(value)
    elif isinstance(node, list):
        for value in node:
            yield from objects(value)


def retire(node, keys):
    """Keeps `keys`' properties (old documents stay valid) but no longer requires them."""
    node['required'] = [key for key in node['required'] if key not in keys]


def extend_store(schema):
    found = 0
    for node in objects(schema):
        properties = node.get('properties')
        if isinstance(properties, dict) and 'archive_enabled' in properties:
            properties['process_history_enabled'] = {'type': 'boolean'}
            retire(node, RETIRED_STORE_KEYS)
            found += 1
    return found


def extend_runtime(schema):
    found = 0
    for node in objects(schema):
        properties = node.get('properties')
        if isinstance(properties, dict) and 'history_workers' in properties:
            retire(node, RETIRED_RUNTIME_KEYS)
            found += 1
    return found


def extend_view_session(schema):
    found = 0
    for node in objects(schema):
        properties = node.get('properties')
        if isinstance(properties, dict) and properties.get('type', {}).get('const') == 'UpdateViewSession':
            properties['overview_bar'] = copy.deepcopy(BAR)
            properties['process_bar'] = copy.deepcopy(BAR)
            found += 1
    return found


def build():
    """The extended schemas by file name."""
    built = {}
    command = json.loads((SPEC / 'command.schema.json').read_text())
    if extend_view_session(command) != 1 or extend_store(command) < 1 or extend_runtime(command) < 1:
        raise SystemExit('command.schema.json changed shape: extensions not applied')
    built['command.schema.json'] = command
    config = json.loads((SPEC / 'config.schema.json').read_text())
    if extend_store(config) != 1 or extend_runtime(config) != 1:
        raise SystemExit('config.schema.json changed shape: extensions not applied')
    built['config.schema.json'] = config
    return {name: json.dumps(schema, indent=2, ensure_ascii=False) + '\n' for name, schema in built.items()}


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--check', action='store_true', help='fail if contracts/ is not up to date')
    args = parser.parse_args()
    stale = []
    for name, text in build().items():
        path = OUT / name
        if args.check:
            if not path.exists() or path.read_text() != text:
                stale.append(name)
        else:
            OUT.mkdir(exist_ok=True)
            path.write_text(text)
    if stale:
        print(f'contracts/ is stale: {", ".join(stale)}; run scripts/build_contracts.py', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
