#!/usr/bin/env python3
"""Write an unsigned coordinated role manifest from the actual staged DEBs.

Signing and qualified publication are separate operations.
"""
import argparse,datetime,hashlib,io,json,subprocess,tarfile
from pathlib import Path
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('directory',type=Path)
parser.add_argument('version')
parser.add_argument('role',choices=['operator','target'])
parser.add_argument('--native-version',default='50.1-0ubuntu2.4+ibara2')
args=parser.parse_args()
names=['ibara'] if args.role=='operator' else ['ibara','libmutter-18-0','mutter-common','mutter-common-bin','gir1.2-mutter-18']
packages=[]
for name in names:
    version=args.version if name=='ibara' else args.native_version
    architecture='all' if name=='mutter-common' else 'amd64'
    prefix='ibara-'+args.role if name=='ibara' else name
    file=args.directory/f'{prefix}_{version}_{architecture}.deb'
    control=subprocess.check_output(['dpkg-deb','-f',str(file),'Package','Version','Architecture'],text=True)
    fields=dict(line.split(': ',1) for line in control.splitlines())
    assert fields=={'Package':name,'Version':version,'Architecture':architecture},fields
    if name=='ibara':
        archive=tarfile.open(fileobj=io.BytesIO(subprocess.check_output(['dpkg-deb','--fsys-tarfile',str(file)])))
        members=[m for m in archive if m.name.lstrip('./')=='usr/lib/ibara/gnome-target.json']
        assert bool(members)==(args.role=='target') and len(members)<=1
        if members:
            marker=members[0]
            assert marker.isfile() and marker.uid==0 and marker.gid==0 and marker.mode==0o644
            assert json.load(archive.extractfile(marker))=={'schema':1,'candidate_version':args.version,'mutter_version':args.native_version,'development_only':'-dev.' in args.version}
    packages.append({'name':name,'file':file.name,'sha256':hashlib.file_digest(file.open('rb'),'sha256').hexdigest(),'size':file.stat().st_size})
value={'schema_version':3,'name':'ibara','version':args.version,'released_at':datetime.datetime.now(datetime.timezone.utc).isoformat(),'platform':{'os':'ubuntu','version':'26.04','architecture':'amd64'},'role':args.role,'native':None if args.role=='operator' else {'version':args.native_version,'mutter_abi':18,'guard_api':1,'helper_api':1},'packages':packages,'notes':['Standalone Ubuntu Console and maintained GNOME target integration.','Signed updates preserve complete role/component sets; graphical activation stays explicit.']}
out=args.directory/f'stable-ubuntu-26.04-amd64-{args.role}.json'
assert not out.exists(),'Preserve the previous manifest before regenerating'
out.write_text(json.dumps(value,indent=2)+'\n')
print(out)
