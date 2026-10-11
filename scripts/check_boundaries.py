#!/usr/bin/env python3
"""Checks the direct production dependencies between dgxtop's workspace crates.

D-79 adds the state, history and compression components, so the spec's
`spec/dgxtop-v2/tools/check_boundaries.py`, which pins the eight crates of the
sealed layout, no longer describes this workspace. This is its successor with
the same rules for the new crate list:

* every implementation crate depends on no workspace crate but `dgxtop-core`,
  except that `dgxtop-history` may use the pure `dgxtop-compression`;
* only the composition root `dgxtop` depends on more;
* crates that must stay pure or free of drivers forbid the same external
  crates the spec's tool forbids, extended to the new components.

It checks direct production edges only (dev dependencies are reviewed
separately), not trait permissions or std::fs use in code.
"""
import argparse
import json
from pathlib import Path

NAMES = {
    'dgxtop-core', 'dgxtop-runtime', 'dgxtop-adapters', 'dgxtop-collectors',
    'dgxtop-state', 'dgxtop-history', 'dgxtop-compression', 'dgxtop-store',
    'dgxtop-manager', 'dgxtop-ui', 'dgxtop',
}
PURE = {'libc', 'nix', 'nvml-wrapper', 'rusqlite', 'ratatui', 'crossterm', 'tokio'}
FORBIDDEN = {
    'dgxtop-core': PURE,
    'dgxtop-compression': PURE,
    'dgxtop-state': PURE,
    'dgxtop-history': PURE,
    'dgxtop-collectors': PURE - {'tokio'},
    'dgxtop-manager': PURE - {'tokio'},
    'dgxtop-ui': {'libc', 'nix', 'nvml-wrapper', 'rusqlite'},
}
# Workspace edges allowed beside dgxtop-core.
ALLOWED = {'dgxtop-history': {'dgxtop-compression'}}


def check(metadata):
    errors = []
    members = set(metadata.get('workspace_members', []))
    packages = [package for package in metadata['packages'] if package.get('id') in members]
    seen = {package['name'] for package in packages}
    if seen != NAMES:
        errors.append(f'workspace names mismatch missing={sorted(NAMES - seen)} extra={sorted(seen - NAMES)}')
    for package in packages:
        name = package['name']
        for dependency in package.get('dependencies', []):
            if dependency.get('kind') == 'dev':
                continue
            target = dependency['name']
            allowed = {'dgxtop-core'} | ALLOWED.get(name, set())
            if target in NAMES and name != 'dgxtop' and target not in allowed:
                errors.append(f'{name} must not depend on {target}')
            if target in FORBIDDEN.get(name, set()):
                errors.append(f'{name} must not depend on {target}')
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('metadata', type=Path, help='output of cargo metadata --format-version 1 --locked')
    args = parser.parse_args()
    try:
        errors = check(json.loads(args.metadata.read_text()))
    except (OSError, ValueError, KeyError, TypeError) as error:
        parser.error(str(error))
    print(json.dumps({'status': 'FAIL' if errors else 'PASS', 'errors': errors}, indent=2))
    return int(bool(errors))


if __name__ == '__main__':
    raise SystemExit(main())
