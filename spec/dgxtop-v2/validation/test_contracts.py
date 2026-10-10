"""契約的負向案例與資料庫約束。"""
import copy,json,sqlite3,unittest
from pathlib import Path
from jsonschema import Draft202012Validator,FormatChecker,ValidationError
from validate_package import u64_validate
ROOT=Path(__file__).resolve().parents[1]
def load(p):return json.loads((ROOT/p).read_text())
class ContractTests(unittest.TestCase):
    def test_unsafe_policy_rejected(self):
        c=load('examples/config.default.json');c['actions']['same_uid_only']=False
        with self.assertRaises(ValidationError):Draft202012Validator(load('contracts/config.schema.json')).validate(c)
    def test_unknown_configuration_rejected(self):
        c=load('examples/config.default.json');c['execute_shell']='echo example'
        with self.assertRaises(ValidationError):Draft202012Validator(load('contracts/config.schema.json')).validate(c)
    def test_failure_cannot_carry_normal_value(self):
        o=load('examples/observation.cpu-package.json');o['read_status']='timeout'
        with self.assertRaises(ValidationError):Draft202012Validator(load('contracts/observation.schema.json')).validate(o)
    def test_conflict_must_be_null(self):
        c=load('examples/canonical.conflict.json');c['value']={'type':'f64','value':55}
        with self.assertRaises(ValidationError):Draft202012Validator(load('contracts/canonical.schema.json')).validate(c)
    def test_negative_pid_rejected(self):
        c=load('examples/command.prepare-signal.json');c['body']['target']['pid']=-1
        with self.assertRaises(ValidationError):Draft202012Validator(load('contracts/command.schema.json')).validate(c)
    def test_u64_semantic_overflow(self):
        with self.assertRaises(AssertionError):u64_validate({'cycle_id':str(2**64)})
    def test_invalid_request_id_rejected(self):
        c=load('examples/command.prepare-signal.json');c['request_id']='not-a-uuid'
        with self.assertRaises(ValidationError):Draft202012Validator(load('contracts/command.schema.json'),format_checker=FormatChecker()).validate(c)
    def test_measurement_freshness_required(self):
        c=load('examples/canonical.conflict.json');del c['measurement_freshness']
        with self.assertRaises(ValidationError):Draft202012Validator(load('contracts/canonical.schema.json')).validate(c)
    def test_stale_measurement_not_selected(self):
        c=load('examples/canonical.conflict.json')
        c.update(selection_state='selected',value={'type':'f64','value':68},source_id='demo',source_generation=1,quality='verified',measurement_freshness='stale')
        with self.assertRaises(ValidationError):Draft202012Validator(load('contracts/canonical.schema.json')).validate(c)
    def test_replay_not_eligible_for_live_history(self):
        c=load('examples/canonical.conflict.json')
        c.update(selection_state='selected',value={'type':'f64','value':68},source_id='demo',source_generation=1,quality='verified',sample_kind='replay',eligible_for_history=True)
        with self.assertRaises(ValidationError):Draft202012Validator(load('contracts/canonical.schema.json')).validate(c)
    def test_nonfinite_semantics_rejected(self):
        with self.assertRaises(AssertionError):u64_validate({'value':float('nan')})
    def test_audit_foreign_key_and_unique(self):
        db=sqlite3.connect(':memory:');db.executescript((ROOT/'contracts/audit.sql').read_text())
        with self.assertRaises(sqlite3.IntegrityError):db.execute('INSERT INTO action_events VALUES(?,?,?,?,?,?,?,?)',('e','missing','2026-10-04','b','00000000000000000001','signal','sent','{}'))
        row=('r','intent','session','{}','SIGTERM',1,'2026-10-04','intent_committed','0'*64,'boot','00000000000000000001')
        db.execute('INSERT INTO action_intents VALUES(?,?,?,?,?,?,?,?,?,?,?)',row)
        with self.assertRaises(sqlite3.IntegrityError):db.execute('INSERT INTO action_intents VALUES(?,?,?,?,?,?,?,?,?,?,?)',row)
        db.close()
    def test_fixed_width_timestamp_constraint(self):
        db=sqlite3.connect(':memory:');db.executescript((ROOT/'contracts/audit.sql').read_text())
        with self.assertRaises(sqlite3.IntegrityError):db.execute('INSERT INTO action_intents VALUES(?,?,?,?,?,?,?,?,?,?,?)',('r','i','s','{}','SIGTERM',1,'date','intent_committed','0'*64,'boot','1'))
        db.close()
    def test_counter_bridge_ddl(self):
        db=sqlite3.connect(':memory:');db.executescript((ROOT/'contracts/history.sql').read_text())
        self.assertEqual(db.execute('PRAGMA user_version').fetchone()[0],2)
        with self.assertRaises(sqlite3.IntegrityError):db.execute('INSERT INTO counter_bridges VALUES(?,?,?,?)',('unknown','00000000000000000059','00000000000000000061','100'))
        db.close()
if __name__=='__main__':unittest.main()
