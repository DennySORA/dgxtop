#!/usr/bin/env python3
"""從現有完整檔案產生MANIFEST與ZIP；不連網、不修改目標repo。"""
import argparse,hashlib,json,zipfile
from pathlib import Path
from validate_package import validate
ROOT=Path(__file__).resolve().parents[1]
def seal(destination: Path):
    validate(False)
    files={}
    for p in sorted(ROOT.rglob('*')):
        if p.is_file() and '__pycache__' not in p.parts and p.name!='MANIFEST.json':
            rel=p.relative_to(ROOT).as_posix()
            files[rel]={'bytes':p.stat().st_size,'sha256':hashlib.sha256(p.read_bytes()).hexdigest()}
    manifest={'document_id':'dgxtop-spec','version':'2.0.0','date':'2026-10-04','source_repository':'DennySORA/dgxtop','source_commit':'b4b5a8d87d3c148cedacf91daf6f926719694a18','scope_status':'COMPLETE','implementation_readiness':'READY_FOR_IMPLEMENTATION','software_acceptance':'NOT_RUN','hardware_certification':'BLOCKED_NOT_MEASURED','requirements':40,'tasks':15,'acceptance_ids':40,'file_count_including_manifest':len(files)+1,'self_hash_policy':'MANIFEST不列自身hash；由外部ZIP SHA256識別完整封裝','files':files}
    (ROOT/'MANIFEST.json').write_text(json.dumps(manifest,ensure_ascii=False,indent=2)+'\n')
    validate(True)
    names=sorted(list(files)+['MANIFEST.json'])
    destination=destination.resolve()
    if destination.is_relative_to(ROOT):raise ValueError('ZIP輸出必須在規格資料夾以外，避免自我納入')
    with zipfile.ZipFile(destination,'w',compression=zipfile.ZIP_DEFLATED,compresslevel=9) as z:
        for rel in names:
            info=zipfile.ZipInfo('dgxtop-spec-v2/'+rel,date_time=(2026,10,4,0,0,0));info.compress_type=zipfile.ZIP_DEFLATED;info.external_attr=0o100644<<16
            z.writestr(info,(ROOT/rel).read_bytes())
    with zipfile.ZipFile(destination) as z:
        assert z.testzip() is None
        assert len(z.infolist())==len(names)
        for rel,entry in files.items():assert hashlib.sha256(z.read('dgxtop-spec-v2/'+rel)).hexdigest()==entry['sha256']
    return {'status':'PASS','zip':str(destination),'files':len(names),'bytes':destination.stat().st_size,'sha256':hashlib.sha256(destination.read_bytes()).hexdigest(),'zip_crc':'PASS','manifest_hashes':'PASS'}
if __name__=='__main__':
    ap=argparse.ArgumentParser(description=__doc__);ap.add_argument('output',type=Path);args=ap.parse_args();print(json.dumps(seal(args.output),ensure_ascii=False,indent=2))
