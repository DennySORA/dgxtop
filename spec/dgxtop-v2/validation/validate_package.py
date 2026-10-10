#!/usr/bin/env python3
"""本地規格完整性、schema、配額、連結與封裝hash檢查；不驗收Rust軟體。"""
import argparse,hashlib,json,math,re,sqlite3,sys
from pathlib import Path
from jsonschema import Draft202012Validator,FormatChecker
from bs4 import BeautifulSoup
ROOT=Path(__file__).resolve().parents[1]
U64_KEYS={'cycle_id','snapshot_revision','start_boot_ns','end_boot_ns','read_started_boot_ns','read_finished_boot_ns','device_sample_boot_ns','observed_boot_ns','committed_boot_ns','start_time_ticks','occurred_boot_ns','deadline_boot_ns','expires_boot_ns','covered_ns'}
def u64_validate(x,key=''):
    if isinstance(x,float):assert math.isfinite(x),'非有限數'
    if isinstance(x,dict):
        for k,v in x.items():
            if k in U64_KEYS and v is not None:assert isinstance(v,str) and v.isdecimal() and 0<=int(v)<=2**64-1,(k,v)
            u64_validate(v,k)
        if x.get('type')=='u64':assert 0<=int(x['value'])<=2**64-1
    elif isinstance(x,list):
        for v in x:u64_validate(v,key)

def config_validate(c):
    Draft202012Validator(json.loads((ROOT/'contracts/config.schema.json').read_text())).validate(c)
    q=c['queues'];r=c['runtime'];m=c['store']['memory_limit_mib']*2**20
    assert q['commands']<=r['max_pending_requests']
    assert q['max_batch_bytes']<=q['max_decoded_batch_bytes']
    assert q['max_batch_bytes']+q['max_decoded_batch_bytes']<=q['ingress_bytes']
    assert q['ingress_bytes']<=m*8/256
    assert q['commands']*65536+q['archive_bytes']+q['log_bytes']+3*2**20<=m*20/256
    assert q['history_requests']>=r['history_workers']
    assert c['viewer']['max_sessions']*2*r['max_projection_bytes']<=m*12/256
    assert r['history_workers']*r['max_history_result_bytes']<=m*16/256
    u64_validate(c)

def validate(require_manifest=True):
    checks=[]
    required=['README.md','SPEC.md','TASKS.md','ACCEPTANCE.md','DECISIONS.md','SOURCES.md','IMPLEMENTATION_PROMPT.md','AGENTS.md','CLAUDE.md','VALIDATION.md','index.html','RUNTIME.md','STORAGE.md','PERFORMANCE.md','UI.md','SECURITY.md','OPERATIONS.md','IMPLEMENTATION_LAYOUT.md','requirements.json']
    for p in required:assert (ROOT/p).is_file() and (ROOT/p).stat().st_size>0,p
    checks.append('必備檔案與非空內容')
    req=json.loads((ROOT/'requirements.json').read_text());assert len(req)==40
    assert {r['id'] for r in req}=={f'R{i:02d}' for i in range(1,41)}
    assert {r['acceptance'] for r in req}=={f'AT{i:02d}' for i in range(1,41)}
    tasks=(ROOT/'TASKS.md').read_text();accept=(ROOT/'ACCEPTANCE.md').read_text();spec=(ROOT/'SPEC.md').read_text()
    for r in req:
        assert r['id'] in spec and f'id="{r["task"].lower()}"' in tasks and f'id="{r["acceptance"].lower()}"' in accept
        assert (ROOT/r['document']).is_file()
    checks.append('40需求／15任務／40驗收映射')
    schemas=list((ROOT/'contracts').glob('*.schema.json'))
    for f in schemas:Draft202012Validator.check_schema(json.loads(f.read_text()))
    mapping={'observation-batch.partial.json':'observation-batch','history-result.partial.json':'history-result','config.default.json':'config','observation.cpu-package.json':'observation','observation.conflicting-source.json':'observation','canonical.conflict.json':'canonical','command.prepare-signal.json':'command','display-module.cpu.json':'display-module','command-result.interval.json':'command-result','history-query.json':'history-query','adapter-manifest.nvml.json':'adapter-manifest','event.gap.json':'event','ipc-request.read.json':'ipc-request'}
    for ex,s in mapping.items():
        x=json.loads((ROOT/'examples'/ex).read_text());Draft202012Validator(json.loads((ROOT/'contracts'/f'{s}.schema.json').read_text()),format_checker=FormatChecker()).validate(x);u64_validate(x)
    config_validate(json.loads((ROOT/'examples/config.default.json').read_text()))
    checks.append(f'{len(schemas)}份JSON Schema／{len(mapping)}份範例／配額語意')
    for f in ['audit.sql','history.sql']:
        db=sqlite3.connect(':memory:');db.executescript((ROOT/'contracts'/f).read_text());assert db.execute('PRAGMA user_version').fetchone()[0]==2;db.close()
    checks.append('SQLite DDL語法與schema_version（非production SQLite版本認證）')
    assert (ROOT/'CLAUDE.md').read_text().strip()=='@AGENTS.md'
    assert '@CLAUDE.md' not in (ROOT/'AGENTS.md').read_text()
    for f in ROOT.rglob('*.md'):
        for link in re.findall(r'\]\(([^)]+)\)',f.read_text()):
            if re.match(r'^(https?://|#|mailto:)',link):continue
            path=link.split('#')[0]
            assert (f.parent/path).resolve().is_file(),f'{f.name}: {link}'
    checks.append('Markdown相對連結／薄CLAUDE匯入')
    html=(ROOT/'index.html').read_text();soup=BeautifulSoup(html,'html.parser');assert soup.html.get('lang')=='zh-Hant-TW'
    ids=[x['id'] for x in soup.select('[id]')];assert len(ids)==len(set(ids)),'重複HTML ID'
    for a in soup.select('[href]'):
        h=a['href']
        if h.startswith('#'):assert h[1:] in ids,h
    assert not soup.select('script[src],link[rel=stylesheet][href],img[src^="http"]')
    for i in range(1,5):assert soup.find(id=f'V{i}') is not None
    assert html.count('<svg')==4
    checks.append('HTML語言、ID、錨點、4幅SVG、無外部依賴')
    if require_manifest:
        manifest=json.loads((ROOT/'MANIFEST.json').read_text())
        actual={str(p.relative_to(ROOT)) for p in ROOT.rglob('*') if p.is_file() and '__pycache__' not in p.parts and p.name!='MANIFEST.json'}
        assert actual==set(manifest['files']),('manifest內容不同',actual-set(manifest['files']),set(manifest['files'])-actual)
        for name,v in manifest['files'].items():
            f=ROOT/name;assert f.stat().st_size==v['bytes'] and hashlib.sha256(f.read_bytes()).hexdigest()==v['sha256'],name
        checks.append('MANIFEST逐檔size／SHA256')
    return checks
if __name__=='__main__':
    ap=argparse.ArgumentParser(description=__doc__);ap.add_argument('--before-seal',action='store_true');args=ap.parse_args()
    try:checks=validate(not args.before_seal)
    except Exception as exc:
        print(json.dumps({'status':'FAIL','error':str(exc)},ensure_ascii=False));raise SystemExit(1)
    print(json.dumps({'status':'PASS','scope':'規格／契約／封裝，不是Rust軟體驗收','checks':checks},ensure_ascii=False,indent=2))
