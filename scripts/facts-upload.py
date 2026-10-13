#!/usr/bin/env python3
"""Upload immutable facts; retain a local file until the archive verifies its hash."""
import argparse, hashlib, json, os, pathlib, urllib.request, urllib.error, urllib.parse
p=argparse.ArgumentParser();p.add_argument('--dir',default=os.environ.get('IBARA_FACTS_DIR',str(pathlib.Path(os.environ.get('IBARA_STATE_DIR',str(pathlib.Path.home()/'.local/state/agent-computer')))/'facts')),type=pathlib.Path);p.add_argument('--url',default=os.environ.get('IBARA_FACTS_URL','https://ibara-facts.mayberrydt.workers.dev'));p.add_argument('--token-file',default=pathlib.Path.home()/'.config/ibara/facts/write.token',type=pathlib.Path);p.add_argument('--keep',action='store_true');a=p.parse_args()
if a.token_file.stat().st_mode & 0o077: p.error('token file must be private (0600)')
token=a.token_file.read_text().strip(); saved=failed=total=0
for file in sorted(a.dir.glob('raw/v1/**/*.jsonl.gz')):
    body=file.read_bytes();key=file.relative_to(a.dir).as_posix();digest=hashlib.sha256(body).hexdigest()
    request=urllib.request.Request(a.url.rstrip('/')+'/v1/archive/'+urllib.parse.quote(key,safe=''),data=body,method='PUT',headers={'Authorization':'Bearer '+token,'Content-Type':'application/gzip','User-Agent':'ibara-facts/1'})
    try:
        with urllib.request.urlopen(request,timeout=30) as r: receipt=json.load(r)
        if receipt.get('saved') is not True or receipt.get('sha256')!=digest: raise ValueError('archive receipt mismatch')
        total+=len(body);saved+=1
        if not a.keep:file.unlink()
    except (OSError,ValueError,urllib.error.HTTPError): failed+=1
print(json.dumps({'saved_objects':saved,'failed_objects':failed,'bytes':total}))
raise SystemExit(1 if failed else 0)
