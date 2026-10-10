"""只測合成規格參考模型，不代表已測 pidfd／驅動／Rust。"""
import unittest
from reference_model import Candidate as C, Counter, arbitrate, normalize_temperature, counter_rate, weighted_window, ActionModel

class ArbitrationTests(unittest.TestCase):
    def test_scope_is_not_interchangeable(self):
        result=arbitrate([C('A','package',68,origin='one'),C('B','soc',42,origin='two')],'package')
        self.assertEqual(result['value'],68);self.assertEqual(result['rejected']['B'],'different_scope')
    def test_alias_does_not_add_vote(self):
        r=arbitrate([C('A','package',68,origin='same'),C('B','package',68,origin='same')],'package')
        self.assertEqual(r['rejected']['B'],'alias_not_independent')
    def test_independent_conflict_is_not_average(self):
        r=arbitrate([C('A','package',68,origin='one'),C('B','package',42,origin='two')],'package')
        self.assertEqual(r['state'],'conflict');self.assertIsNone(r['value'])
    def test_fault_falls_back(self):
        r=arbitrate([C('A','package',68,origin='one',fault=True),C('B','package',67,origin='two')],'package')
        self.assertEqual(r['source'],'B')
    def test_zero_is_real_value(self):
        self.assertEqual(arbitrate([C('A','package',0)],'package')['value'],0)
    def test_unknown_unit_is_not_guessed(self):
        self.assertIsNone(normalize_temperature(68000,'mV'))
    def test_non_finite_is_rejected(self):
        self.assertIsNone(normalize_temperature(float('nan'),'degC'))
    def test_native_unit_conversion(self):
        self.assertEqual(normalize_temperature(68000,'mdegC'),68)
    def test_stale_is_not_selected(self):
        self.assertEqual(arbitrate([C('A','package',68,age_ms=3001)],'package')['state'],'unavailable')
    def test_constant_does_not_imply_fake(self):
        self.assertTrue(all(arbitrate([C('A','package',68)],'package')['state']=='selected' for _ in range(20)))
    def test_alarm_does_not_drop_hot_reading(self):
        self.assertEqual(arbitrate([C('A','package',99,alarm=True)],'package')['value'],99)
    def test_pin_cannot_override_fault(self):
        self.assertEqual(arbitrate([C('A','package',68,fault=True)],'package',pin='A')['reason'],'pinned_source_unavailable')
    def test_pin_cannot_hide_conflict(self):
        r=arbitrate([C('A','package',68,origin='a'),C('B','package',42,origin='b')],'package',pin='A')
        self.assertEqual(r['state'],'conflict')
    def test_simulated_not_live(self):
        self.assertEqual(arbitrate([C('A','package',68)],'package',namespace='measured')['state'],'unavailable')
    def test_close_values_use_profile_priority(self):
        r=arbitrate([C('A','package',68,rank=2),C('B','package',67,rank=1)],'package')
        self.assertEqual(r['source'],'B')

class HistoryTests(unittest.TestCase):
    def test_real_elapsed_time(self):
        self.assertEqual(counter_rate(Counter(100,0,'b','s',1),Counter(400,2_000_000_000,'b','s',1)),150)
    def test_config_not_input_to_existing_delta(self):
        a=Counter(100,0,'b','s',1);b=Counter(400,2_000_000_000,'b','s',1)
        for configured_interval in [0.1,0.5,1,10]:self.assertEqual(counter_rate(a,b),150)
    def test_reset_is_warmup(self):
        self.assertIsNone(counter_rate(Counter(100,0,'b','s',1),Counter(1,1_000_000_000,'b','s',1)))
    def test_source_change_is_warmup(self):
        self.assertIsNone(counter_rate(Counter(100,0,'b','s',1),Counter(200,1_000_000_000,'b','t',1)))
    def test_boot_change_is_warmup(self):
        self.assertIsNone(counter_rate(Counter(100,0,'b','s',1),Counter(200,1_000_000_000,'c','s',1)))
    def test_long_gap_excludes_old_data(self):
        r=weighted_window([(0,100),(7200,20)],3601,7201,3)
        self.assertEqual(r['mean'],20);self.assertEqual(r['covered'],1)
    def test_empty_is_missing(self):
        self.assertIsNone(weighted_window([],0,3600,3)['mean'])

class ActionPolicyModelTests(unittest.TestCase):
    def test_prepare_does_not_send(self):
        m=ActionModel();self.assertEqual(m.prepare('p',1000,1000,0),'prepared');self.assertEqual(m.sent_count,0)
    def test_wrong_uid_and_root_denied(self):
        for uid,target in [(1000,1001),(0,0)]:self.assertEqual(ActionModel().prepare('p',uid,target,0),'denied')
    def test_changed_identity_denied(self):
        m=ActionModel();m.prepare('old',1000,1000,0);self.assertEqual(m.confirm('new',1,'r'),'denied');self.assertEqual(m.sent_count,0)
    def test_expired_denied(self):
        m=ActionModel();m.prepare('p',1000,1000,0);self.assertEqual(m.confirm('p',10,'r'),'denied')
    def test_audit_failure_denied(self):
        m=ActionModel();m.prepare('p',1000,1000,0);self.assertEqual(m.confirm('p',1,'r',False),'denied');self.assertEqual(m.sent_count,0)
    def test_duplicate_is_idempotent(self):
        m=ActionModel();m.prepare('p',1000,1000,0);m.confirm('p',1,'r');m.confirm('p',2,'r');self.assertEqual(m.sent_count,1)
    def test_used_token_with_new_request_denied(self):
        m=ActionModel();m.prepare('p',1000,1000,0);m.confirm('p',1,'r');self.assertEqual(m.confirm('p',2,'s'),'denied')
    def test_unsupported_is_not_pid_fallback(self):
        self.assertEqual(ActionModel().prepare('p',1000,1000,0,supported=False),'denied')
if __name__=='__main__':unittest.main()
