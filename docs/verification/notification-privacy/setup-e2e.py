import pathlib,subprocess,os,json,hashlib,shutil
p=pathlib.Path.cwd(); home=pathlib.Path.home(); result=[]
# Risks: fresh setup fails to link; Marketplace tree is moved/overwritten;
# independent symlink is replaced; repeat setup changes ownership/content.
# Run the real CLI with an isolated home and stub only service/shell control.
stubs=p/'setup-stubs';stubs.mkdir(exist_ok=True)
for command,body in [('systemctl','exit 0'),('qs','echo "[]"'),('sudo','exit 0'),('omarchy-shell','exit 1')]:
 f=stubs/command;f.write_text('#!/bin/sh\n'+body+'\n');f.chmod(0o755)
(p/'setup-runtime').mkdir(exist_ok=True)
lib=p/'unpacked/usr/lib/ibara'
for case in ['fresh','marketplace','independent-link']:
 h=p/('home-'+case);h.mkdir(exist_ok=True); plugins=h/'.config/omarchy/plugins';plugins.mkdir(parents=True,exist_ok=True); link=plugins/'io.zet.ibara'
 if case=='marketplace':
  link.mkdir();(link/'Service.qml').write_text('fixed Marketplace canary'); before=(link/'Service.qml').read_bytes()
 if case=='independent-link':
  other=h/'independent';other.mkdir();(other/'Service.qml').write_text('independent plugin');link.symlink_to(home/'independent')
 cmd=['bwrap','--ro-bind','/','/','--dev','/dev','--proc','/proc','--tmpfs','/tmp','--tmpfs','/home','--bind',str(h),str(home),'--bind',str(p),'/tmp/work','--ro-bind',str(lib),'/usr/lib/ibara','--setenv','HOME',str(home),'--setenv','PATH','/tmp/work/setup-stubs:/usr/bin','--setenv','XDG_RUNTIME_DIR','/tmp/work/setup-runtime','--setenv','XDG_CONFIG_HOME',str(home/'.config'),'--setenv','XDG_STATE_HOME',str(home/'.local/state'),'--unsetenv','DBUS_SESSION_BUS_ADDRESS','/tmp/work/unpacked/usr/lib/ibara/bin/ibara','setup','--role','operator']
 for attempt in range(2):
  r=subprocess.run(cmd,text=True,capture_output=True,timeout=30);(p/f'setup-{case}-{attempt}.log').write_text(r.stdout+r.stderr);assert r.returncode==0,(case,r.stdout,r.stderr)
 if case=='fresh':assert link.is_symlink() and os.readlink(link)=='/usr/share/ibara/omarchy-plugin'
 elif case=='marketplace':assert not link.is_symlink() and (link/'Service.qml').read_bytes()==before and not list(plugins.glob('.*marketplace*'))
 else:assert os.readlink(link)==str(home/'independent')
 u=subprocess.run(cmd[:-3]+['uninstall','--yes'],text=True,capture_output=True,timeout=30)
 (p/f'uninstall-{case}.log').write_text(u.stdout+u.stderr);assert u.returncode==0,(case,u.stdout,u.stderr)
 if case=='fresh':assert not link.is_symlink()
 elif case=='marketplace':assert (link/'Service.qml').read_bytes()==before
 else:assert os.readlink(link)==str(home/'independent')
 result.append({'case':case,'setup_runs':2,'uninstall_preserves_ownership':True,'passed':True})
(p/'setup-results.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result))
