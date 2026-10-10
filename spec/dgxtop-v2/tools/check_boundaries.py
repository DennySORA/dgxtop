#!/usr/bin/env python3
"""檢查 cargo metadata 的直接 production workspace依賴；非執行期sandbox。"""
import argparse
import json
from pathlib import Path
NAMES={'dgxtop-core','dgxtop-runtime','dgxtop-adapters','dgxtop-collectors','dgxtop-store','dgxtop-manager','dgxtop-ui','dgxtop'}
FORBIDDEN={'dgxtop-core':{'libc','nix','nvml-wrapper','rusqlite','ratatui','crossterm','tokio'},'dgxtop-ui':{'libc','nix','nvml-wrapper','rusqlite'},'dgxtop-manager':{'libc','nix','nvml-wrapper','rusqlite','ratatui','crossterm'},'dgxtop-collectors':{'libc','nix','nvml-wrapper','rusqlite','ratatui','crossterm'}}

def check(metadata):
    errors=[];members=set(metadata.get('workspace_members',[]))
    packages=[p for p in metadata['packages'] if p.get('id') in members]
    seen={p['name'] for p in packages}
    if seen!=NAMES:errors.append(f'workspace names mismatch missing={sorted(NAMES-seen)} extra={sorted(seen-NAMES)}')
    for p in packages:
        name=p['name']
        for dep in p.get('dependencies',[]):
            # Integration dev dependencies reviewed separately; not a production edge.
            if dep.get('kind')=='dev':continue
            target=dep['name']
            if target in NAMES and name!='dgxtop' and target!='dgxtop-core':
                errors.append(f'{name} 不得直接依賴 {target}')
            if target in FORBIDDEN.get(name,set()):errors.append(f'{name} 禁止依賴 {target}')
    return errors

def main():
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument('metadata',type=Path,help='cargo metadata --format-version 1 --locked 輸出')
    args=ap.parse_args()
    try: errors=check(json.loads(args.metadata.read_text()))
    except (OSError,ValueError,KeyError,TypeError) as exc:ap.error(str(exc))
    print(json.dumps({'status':'FAIL' if errors else 'PASS','errors':errors,'limitations':'只檢查直接production dependency，不代替Rust trait權限與非法std::fs/process程式碼審核'},ensure_ascii=False,indent=2))
    return int(bool(errors))
if __name__=='__main__':raise SystemExit(main())
