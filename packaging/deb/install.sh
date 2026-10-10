#!/usr/bin/env bash
# Render release.env placeholders before publishing as install-ubuntu.
set -euo pipefail
IBARA_BASE_URL='@IBARA_BASE_URL@'
IBARA_RELEASE_KEY='@IBARA_RELEASE_KEY@'
temporary=''
main() {
  local role=${1:-operator}
  [[ $# -le 1 && ($role == operator || $role == target) ]] || { echo 'Usage: install-ubuntu [operator|target]' >&2; return 1; }
  [[ $(id -u) != 0 ]] || { echo 'Run this as your desktop user.' >&2; return 1; }
  . /etc/os-release
  [[ $ID == ubuntu && $VERSION_ID == 26.04 && $(dpkg --print-architecture) == amd64 ]] || { echo 'Requires Ubuntu 26.04 amd64.' >&2; return 1; }
  for tool in curl python3 ssh-keygen dpkg-deb sudo; do
    command -v "$tool" >/dev/null || { echo "Missing $tool. Install curl python3 openssh-client sudo first." >&2; return 1; }
  done
  if [[ $role == operator && -e /usr/lib/ibara/gnome-target.json ]]; then
    echo 'This computer has target support. Use its target channel; operator installation would remove that role.' >&2
    return 1
  fi
  local manifest
  temporary=$(mktemp -d)
  trap 'rm -rf -- "$temporary"' EXIT
  umask 077
  manifest=stable-ubuntu-26.04-amd64-$role.json
  fetch() { curl -fsSL --proto '=https' --proto-redir '=https' --connect-timeout 15 --max-filesize 536870912 -o "$temporary/$1" "$IBARA_BASE_URL/$1"; }
  fetch "$manifest"
  fetch "$manifest.sig"
  printf 'ibara-release namespaces="ibara-release" %s\n' "$IBARA_RELEASE_KEY" > "$temporary/allowed-signers"
  ssh-keygen -Y verify -f "$temporary/allowed-signers" -I ibara-release -n ibara-release -s "$temporary/$manifest.sig" < "$temporary/$manifest" >/dev/null
  python3 - "$temporary/$manifest" "$role" > "$temporary/assets" <<'PY'
import json,re,sys
m=json.load(open(sys.argv[1]));role=sys.argv[2]
assert m['schema_version']==3 and m['name']=='ibara'
assert m['platform']=={'os':'ubuntu','version':'26.04','architecture':'amd64'} and m['role']==role
assert re.fullmatch(r'[0-9][A-Za-z0-9.+:-]*',m['version']) and '-dev.' not in m['version']
native='50.1-0ubuntu2.4+ibara2'
names=['ibara'] if role=='operator' else ['ibara','libmutter-18-0','mutter-common','mutter-common-bin','gir1.2-mutter-18']
assert len(m['packages'])==len(names) and sorted(p['name'] for p in m['packages'])==sorted(names)
assert m.get('native')==(None if role=='operator' else {'version':native,'mutter_abi':18,'guard_api':1,'helper_api':1})
for p in m['packages']:
    version=m['version'] if p['name']=='ibara' else native
    architecture='all' if p['name']=='mutter-common' else 'amd64'
    prefix='ibara-'+role if p['name']=='ibara' else p['name']
    assert p['file']==f'{prefix}_{version}_{architecture}.deb'
    assert re.fullmatch(r'[0-9a-f]{64}',p['sha256']) and isinstance(p['size'],int) and 0<p['size']<=536870912
    print(p['file'])
PY
  local file
  while IFS= read -r file; do fetch "$file"; done < "$temporary/assets"
  python3 - "$temporary" "$manifest" "$role" <<'PY'
import hashlib,json,pathlib,subprocess,sys
root=pathlib.Path(sys.argv[1]);m=json.loads((root/sys.argv[2]).read_text());role=sys.argv[3]
for p in m['packages']:
    file=root/p['file']
    assert file.stat().st_size==p['size'] and hashlib.file_digest(file.open('rb'),'sha256').hexdigest()==p['sha256']
    control=subprocess.check_output(['dpkg-deb','-f',str(file),'Package','Version','Architecture'],text=True)
    fields=dict(line.split(': ',1) for line in control.splitlines())
    assert fields=={'Package':p['name'],'Version':m['version'] if p['name']=='ibara' else m['native']['version'],'Architecture':'all' if p['name']=='mutter-common' else 'amd64'}
    if p['name']=='ibara':
        contents=subprocess.check_output(['dpkg-deb','-c',str(file)],text=True)
        target=any(line.split()[-1:] == ['./usr/lib/ibara/gnome-target.json'] for line in contents.splitlines())
        assert target==(role=='target')
PY
  local packages=() pairs=()
  while IFS= read -r file; do
    packages+=("$temporary/$file")
    pairs+=("$temporary/$file" "$(sha256sum "$temporary/$file" | cut -d' ' -f1)")
  done < "$temporary/assets"
  # Established targets must pass the installed root busy gate before apt effects.
  # Initial bootstrap has no target controller to protect.
  if [[ ! -e /usr/lib/ibara/gnome-target.json ]]; then
    sudo apt-get install --yes --no-remove -- "${packages[@]}"
  fi
  # Installed code rechecks signatures and bytes as root and caches the complete set.
  sudo /usr/bin/ibara system update "$(id -un)" --signed-set "$temporary/$manifest" "$temporary/$manifest.sig" "${pairs[@]}"
  if [[ $role == operator ]]; then
    /usr/bin/ibara setup --role operator
  else
    echo 'Packages installed. Enable ibara@zet.io, sign out and back in to load Mutter, then run: ibara setup --role both'
    echo 'No compositor restart or Tailscale enrollment was performed.'
  fi
}
main "$@"
