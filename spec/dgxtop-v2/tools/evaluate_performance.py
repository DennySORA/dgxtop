#!/usr/bin/env python3
"""評估實測輸入的門檻及證據完整性；不量測硬體、不保證輸入未造假。"""
import argparse
import hashlib
import json
from pathlib import Path
from jsonschema import Draft202012Validator, FormatChecker
ROOT = Path(__file__).resolve().parents[1]

def evaluate(report: dict, evidence_dir: Path) -> list[str]:
    schema = json.loads((ROOT/'contracts/performance-report.schema.json').read_text())
    errors = [f"schema:{'.'.join(map(str,e.path))}: {e.message}" for e in Draft202012Validator(schema,format_checker=FormatChecker()).iter_errors(report)]
    if errors:
        return errors
    import math
    def finite(x):
        if isinstance(x,float) and not math.isfinite(x): return False
        if isinstance(x,dict): return all(finite(v) for v in x.values())
        if isinstance(x,list): return all(finite(v) for v in x)
        return True
    if not finite(report): return ['報告含非有限數']
    if report['kind'] != 'measured':
        errors.append('合成範例不能作為實測驗收')
    budget = json.loads((ROOT/'bench/budgets.json').read_text())
    import math
    def gates(values, limits, label):
        for key, bound in limits.items():
            value = values[key]
            if not math.isfinite(value):
                errors.append(f'{label}:{key}: 非有限數');continue
            if ('max' in bound and value>bound['max']) or ('min' in bound and value<bound['min']):
                errors.append(f'{label}:{key}={value} 超出 {bound}')
    for i,run in enumerate(report['runs'],1):
        if run['duration_seconds']<budget['minimum_duration_seconds']:
            errors.append(f'REF{i}: 測量時間不足')
        for k,n in budget['minimum_samples'].items():
            if run['sample_counts'][k]<n: errors.append(f'REF{i}: {k} 樣本不足')
        gates(run['metrics'],budget['limits'],f'REF{i}')
    if {x['scenario'] for x in report['soak_runs']} != {'fixed','churn'}:
        errors.append('soak 必須包含 fixed 與 churn，不能重複同一案例')
    for item in report['soak_runs']:
        gates(item,budget['soak_limits'],'SOAK:'+item['scenario'])
    base=evidence_dir.resolve()
    cfg=(base/report['config_path']).resolve()
    if Path(report['config_path']).is_absolute() or not cfg.is_relative_to(base) or not cfg.is_file():
        errors.append('設定證據檔缺失或越界')
    else:
        if hashlib.sha256(cfg.read_bytes()).hexdigest()!=report['config_sha256']:errors.append('設定檔hash不符')
        if json.loads(cfg.read_text())!=json.loads((ROOT/'examples/config.default.json').read_text()):errors.append('REF設定不等於規定預設工作量設定')
    for ref in report['evidence_files']:
        path=(base/ref['path']).resolve()
        if Path(ref['path']).is_absolute() or not path.is_relative_to(base):
            errors.append('證據路徑越界');continue
        if not path.is_file(): errors.append('缺少證據檔：'+ref['path']);continue
        if hashlib.sha256(path.read_bytes()).hexdigest()!=ref['sha256']:
            errors.append('證據雜湊不符：'+ref['path'])
    return errors

def main() -> int:
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument('report',type=Path);ap.add_argument('--evidence-dir',type=Path,required=True)
    args=ap.parse_args()
    try: errors=evaluate(json.loads(args.report.read_text()),args.evidence_dir)
    except (OSError,ValueError,KeyError,TypeError) as exc:
        print(json.dumps({'status':'INVALID_INPUT','error':str(exc)},ensure_ascii=False));return 2
    print(json.dumps({'status':'FAIL' if errors else 'PASS','errors':errors,'scope':'僅輸入報告的門檻／證據檔檢查，不驗證實測真實性'},ensure_ascii=False,indent=2))
    return 1 if errors else 0
if __name__=='__main__':raise SystemExit(main())
