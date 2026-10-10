import unittest
from runtime_model import Ledger,next_tick,counter_interval,weighted_gauge,Action,Pressure
class RuntimeTests(unittest.TestCase):
 def test_byte_budget(self):
  q=Ledger(8,32);self.assertTrue(q.acquire('a',5));self.assertFalse(q.acquire('b',5));self.assertEqual(q.used,5)
 def test_item_budget(self):
  q=Ledger(100,1);q.acquire('a',1);self.assertFalse(q.acquire('b',1))
 def test_release(self):
  q=Ledger(8,32);q.acquire('a',5);q.release('a');self.assertEqual(q.used,0)
 def test_double_release(self):
  with self.assertRaises(ValueError):Ledger(8,32).release('missing')
 def test_skip_ticks(self):self.assertEqual(next_tick(0,1000,5555),6000)
 def test_no_immediate_replay(self):self.assertEqual(next_tick(0,1000,6000),7000)
 def test_counter_exact(self):self.assertEqual(counter_interval(1000,1300,0,3,0,3),(300,3,'counter_delta'))
 def test_counter_bridge(self):self.assertEqual(counter_interval(1000,1300,59,61,0,120),(300,2,'counter_delta'))
 def test_counter_partial(self):self.assertEqual(counter_interval(1000,1300,59,61,60,120),(None,0,'boundary_gap'))
 def test_counter_reset(self):self.assertEqual(counter_interval(1300,1000,0,3,0,3)[2],'reset')
 def test_time_weighting(self):self.assertEqual(weighted_gauge([(0,1,10),(1,4,30)],0,4),(25,1.0))
 def test_coverage_gap(self):self.assertEqual(weighted_gauge([(0,1,10),(3,4,30)],0,4),(20,.5))
 def test_overlap(self):
  with self.assertRaises(ValueError):weighted_gauge([(0,2,10),(1,3,30)],0,4)
 def test_once(self):
  a=Action(100,1);self.assertTrue(a.confirm(0,1));self.assertTrue(a.durable_ack(1,1));self.assertFalse(a.durable_ack(2,1));self.assertEqual(a.send_count,1)
 def test_cancel_pending(self):
  a=Action(100,1);a.confirm(0,1);a.cancel();self.assertFalse(a.durable_ack(1,1));self.assertEqual(a.send_count,0)
 def test_late_ack(self):
  a=Action(100,1);a.confirm(0,1);self.assertFalse(a.durable_ack(100,1));self.assertEqual(a.send_count,0)
 def test_policy_change(self):
  a=Action(100,1);a.confirm(0,1);self.assertFalse(a.durable_ack(10,2))
 def test_crash_no_retry(self):
  a=Action(100,1);a.confirm(0,1);a.durable_ack(1,1);a.crash();self.assertFalse(a.confirm(2,1));self.assertEqual(a.send_count,1)
 def test_pressure_hysteresis(self):
  p=Pressure();p.update(.8,.6,0);p.update(.8,.6,1000);self.assertEqual(p.state,'normal');p.update(.8,.6,2000);self.assertEqual(p.state,'normal');p.update(.8,.6,3000);self.assertEqual(p.state,'pressured')
 def test_recovery(self):
  p=Pressure();p.update(.1,.9,0);p.update(.1,.5,1000);self.assertEqual(p.update(.1,.5,31000),'recovering');self.assertEqual(p.update(.1,.5,61000),'normal')
if __name__=='__main__':unittest.main()
