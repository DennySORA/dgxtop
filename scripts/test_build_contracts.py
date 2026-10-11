"""Tests of scripts/build_contracts.py (D-80)."""
import json
import unittest

import build_contracts


class BuildContractsTest(unittest.TestCase):
    def test_committed_contracts_are_up_to_date(self):
        self.assertEqual(build_contracts.main.__module__, 'build_contracts')
        for name, text in build_contracts.build().items():
            committed = (build_contracts.OUT / name).read_text()
            self.assertEqual(committed, text, f'{name} is stale')

    def test_only_the_listed_fields_are_added(self):
        built = {name: json.loads(text) for name, text in build_contracts.build().items()}
        spec = json.loads((build_contracts.SPEC / 'command.schema.json').read_text())
        extended = built['command.schema.json']
        variants = extended['properties']['body']['oneOf']
        update = next(v for v in variants if v['properties']['type'].get('const') == 'UpdateViewSession')
        spec_update = next(v for v in spec['properties']['body']['oneOf'] if v['properties']['type'].get('const') == 'UpdateViewSession')
        added = set(update['properties']) - set(spec_update['properties'])
        self.assertEqual(added, {'overview_bar', 'process_bar'})
        self.assertEqual(update['required'], spec_update['required'], 'the new fields are optional')
        store = built['config.schema.json']['properties']['store']
        self.assertIn('process_history_enabled', store['properties'])
        self.assertNotIn('process_history_enabled', store['required'])


if __name__ == '__main__':
    unittest.main()
