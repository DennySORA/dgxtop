"""效能檢查器的測試輸入是假報告，只測檢查器，不代表真實效能。"""
import copy,hashlib,json,sys,tempfile,unittest
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1];sys.path.insert(0,str(ROOT/'tools'))
from evaluate_performance import evaluate
from generate_fixture import generate
from check_boundaries import check,NAMES

def report():
 b=json.loads((ROOT/'bench/budgets.json').read_text());sc=json.loads((ROOT/'bench/scenarios.json').read_text())['profiles'][0]
 return {'schema_version':1,'kind':'measured','profile':'REF','source_commit':'a'*40,'config_sha256':hashlib.sha256((ROOT/'examples/config.default.json').read_bytes()).hexdigest(),'config_path':'config.json','environment':{'cpu':'test-only fake CPU','architecture':'x86_64','os_kernel':'synthetic','logical_cpus':4,'ram_mib':16384,'rustc':'synthetic','sqlite':'synthetic','gpu_driver':'synthetic','filesystem':'synthetic','terminal':'synthetic','background_load':'synthetic'},'workload':{k:v for k,v in sc.items() if k not in ('id','duration_seconds')},'runs':[{'duration_seconds':1800,'sample_counts':dict(b['minimum_samples']),'metrics':{k:v.get('min',0) for k,v in b['limits'].items()}} for _ in range(3)],'soak_runs':[dict({k:v.get('min',0) for k,v in b['soak_limits'].items()},scenario=x) for x in ['fixed','churn']],'evidence_files':[{'path':'fake.txt','sha256':hashlib.sha256(b'unit test only').hexdigest()}]}
class ToolsTests(unittest.TestCase):
 def setUp(self):
  self.tmp=tempfile.TemporaryDirectory();self.path=Path(self.tmp.name);(self.path/'fake.txt').write_bytes(b'unit test only');(self.path/'config.json').write_bytes((ROOT/'examples/config.default.json').read_bytes())
 def tearDown(self):self.tmp.cleanup()
 def test_checker_accepts_threshold_fixture(self):self.assertEqual(evaluate(report(),self.path),[])
 def test_rejects_synthetic_label(self):
  r=report();r['kind']='synthetic_example';self.assertTrue(evaluate(r,self.path))
 def test_each_run_must_pass(self):
  r=report();r['runs'][2]['metrics']['cpu_mean_one_core_percent']=5.1;self.assertTrue(evaluate(r,self.path))
 def test_insufficient_duration(self):
  r=report();r['runs'][0]['duration_seconds']=1799;self.assertTrue(evaluate(r,self.path))
 def test_insufficient_sample_count(self):
  r=report();r['runs'][0]['sample_counts']['commands']=9999;self.assertTrue(evaluate(r,self.path))
 def test_wrong_workload(self):
  r=report();r['workload']['adapter_hosts']=1;self.assertTrue(evaluate(r,self.path))
 def test_failed_coverage(self):
  r=report();r['runs'][0]['metrics']['coverage_percent']=99;self.assertTrue(evaluate(r,self.path))
 def test_no_adaptive_escape(self):
  r=report();r['runs'][0]['metrics']['adaptive_throttle_events']=1;self.assertTrue(evaluate(r,self.path))
 def test_duplicate_soak(self):
  r=report();r['soak_runs'][1]['scenario']='fixed';self.assertTrue(evaluate(r,self.path))
 def test_missing_proof(self):
  r=report();r['evidence_files'][0]['path']='missing';self.assertTrue(evaluate(r,self.path))
 def test_wrong_hash(self):
  r=report();r['evidence_files'][0]['sha256']='0'*64;self.assertTrue(evaluate(r,self.path))
 def test_traversal(self):
  r=report();r['evidence_files'][0]['path']='../fake.txt';self.assertTrue(evaluate(r,self.path))
 def test_config_mismatch(self):
  r=report();r['config_sha256']='b'*64;self.assertTrue(evaluate(r,self.path))
 def test_nonfinite_duration(self):
  r=report();r['runs'][0]['duration_seconds']=float('nan');self.assertTrue(evaluate(r,self.path))
 def test_generator_deterministic(self):self.assertEqual(list(generate(2,2,1)),list(generate(2,2,1)))
 def test_generator_limit(self):
  with self.assertRaises(ValueError):list(generate(4096,10000,1))
 def test_generator_synthetic(self):self.assertTrue(all(x['sample_kind']=='simulated' for x in generate(2,2,1)))
 def test_dependency_graph(self):
  m={'workspace_members':list(NAMES),'packages':[{'name':n,'id':n,'dependencies':[]} for n in NAMES]};self.assertEqual(check(m),[])
 def test_invalid_dependency(self):
  m={'workspace_members':list(NAMES),'packages':[{'name':n,'id':n,'dependencies':[{'name':'dgxtop-adapters','kind':None}] if n=='dgxtop-ui' else []} for n in NAMES]};self.assertTrue(check(m))
if __name__=='__main__':unittest.main()
