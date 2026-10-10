#!/usr/bin/env python3
"""產生可重現合成 Observation JSONL；沒有任何 driver I/O 或 action。"""
import argparse
import json
import random
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]

def generate(series: int, cycles: int, seed: int):
    if not 1<=series<=4096 or not 1<=cycles<=10000 or series*cycles*4>1000000:
        raise ValueError('超出 fixture 的有界產生範圍（最多100萬筆）')
    rng=random.Random(seed)
    prototype=json.loads((ROOT/'examples/observation.cpu-package.json').read_text())
    for cycle in range(cycles):
        for key in range(series):
            # Four candidates, paired aliases: two physically independent origins.
            base=40000+rng.randrange(20000)
            for source in range(4):
                item=json.loads(json.dumps(prototype))
                item['cycle_id']=str(cycle+1)
                item['key']['entity_id']=f'cpu.package.{key}'
                item['key']['scope']=f'package:{key}'
                item['provenance'].update(source_id=f'fixture:{key}:{source}',origin_id=f'fixture-sensor:{key}:{source//2}',generation=1)
                item['value']['value']=base+(source//2)*100
                item['sample_kind']='simulated'
                start=(cycle+1)*1000000000
                item.update(read_started_boot_ns=str(start),read_finished_boot_ns=str(start+1000000),device_sample_boot_ns=str(start))
                item['reason_codes']=['SYNTHETIC_FIXTURE','NOT_HARDWARE_EVIDENCE']
                yield item

def main():
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--series',type=int,default=8);ap.add_argument('--cycles',type=int,default=3)
    ap.add_argument('--seed',type=int,default=20261004);ap.add_argument('--out',type=Path,required=True)
    args=ap.parse_args()
    if args.out.exists():ap.error('輸出已存在；請明確換一個檔名')
    try:
        iterator=generate(args.series,args.cycles,args.seed)
        first=next(iterator)
        with args.out.open('x',encoding='utf-8') as f:
            f.write(json.dumps(first,ensure_ascii=False,separators=(',',':'))+'\n')
            for row in iterator:f.write(json.dumps(row,ensure_ascii=False,separators=(',',':'))+'\n')
    except (ValueError,OSError) as exc:ap.error(str(exc))
if __name__=='__main__':main()
