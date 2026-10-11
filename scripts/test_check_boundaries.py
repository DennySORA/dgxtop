"""Tests of scripts/check_boundaries.py (D-79)."""
import unittest

import check_boundaries


def metadata(edges, names=check_boundaries.NAMES):
    packages = []
    for name in sorted(names):
        packages.append({
            'id': name,
            'name': name,
            'dependencies': [{'name': target, 'kind': kind} for source, target, kind in edges if source == name],
        })
    return {'workspace_members': sorted(names), 'packages': packages}


class CheckBoundariesTest(unittest.TestCase):
    def test_core_edges_pass(self):
        edges = [(name, 'dgxtop-core', None) for name in check_boundaries.NAMES if name != 'dgxtop-core']
        edges.append(('dgxtop', 'dgxtop-store', None))
        self.assertEqual(check_boundaries.check(metadata(edges)), [])

    def test_history_may_use_compression_and_nothing_else_may(self):
        self.assertEqual(check_boundaries.check(metadata([('dgxtop-history', 'dgxtop-compression', None)])), [])
        errors = check_boundaries.check(metadata([('dgxtop-ui', 'dgxtop-compression', None)]))
        self.assertEqual(errors, ['dgxtop-ui must not depend on dgxtop-compression'])
        errors = check_boundaries.check(metadata([('dgxtop-history', 'dgxtop-store', None)]))
        self.assertEqual(errors, ['dgxtop-history must not depend on dgxtop-store'])

    def test_the_manager_uses_neither_compression_nor_store(self):
        errors = check_boundaries.check(metadata([('dgxtop-manager', 'dgxtop-compression', None)]))
        self.assertEqual(errors, ['dgxtop-manager must not depend on dgxtop-compression'])
        errors = check_boundaries.check(metadata([('dgxtop-manager', 'dgxtop-store', None)]))
        self.assertEqual(errors, ['dgxtop-manager must not depend on dgxtop-store'])

    def test_pure_components_refuse_drivers_and_sqlite(self):
        for name in ('dgxtop-state', 'dgxtop-history', 'dgxtop-compression'):
            errors = check_boundaries.check(metadata([(name, 'rusqlite', None)]))
            self.assertEqual(errors, [f'{name} must not depend on rusqlite'])

    def test_dev_edges_and_the_crate_list(self):
        self.assertEqual(check_boundaries.check(metadata([('dgxtop-ui', 'dgxtop-store', 'dev')])), [])
        missing = check_boundaries.check(metadata([], names=check_boundaries.NAMES - {'dgxtop-state'}))
        self.assertTrue(missing[0].startswith('workspace names mismatch'), missing)


if __name__ == '__main__':
    unittest.main()
