#!/usr/bin/env bash
# Build, check and upload one ibara release as a GitHub release of the
# repository packaging/release.env names.
#
#   packaging/publish.sh [--publish] [--sign-key KEY] [--private-markers FILE] [--out DIR] [--built VERIFIED_DIR] \
#     --plugin PLUGIN_CHECKOUT --stream IBARA_STREAM_CHECKOUT --view IBARA_VIEW_CHECKOUT \
#     --notes NOTES_FILE VERSION
#
# VERSION is the full release version, PKGVER-PKGREL (for example 0.1.0-1):
# PKGVER must be packaging/PKGBUILD's pkgver, and PKGREL is the release
# number. The release is tagged vVERSION at this checkout's commit, which must
# already be pushed to the repository.
#
# 1. packaging/release.sh builds and signs the release into DIR (default
#    ~/.cache/ibara-release/VERSION, which must not hold anything yet).
# 2. Every file is checked again: the manifest's signature against
#    IBARA_RELEASE_KEY, each package's digest, size and version, the channel
#    built into the ibara package, and the installer.
# 3. `gh release create` uploads the three packages, the source tarball,
#    install, stable.json and stable.json.sig, then each uploaded file is
#    compared with its local copy by size and SHA-256.
#
# Without --publish the release is a draft: nobody sees it, and installs and
# updates keep getting the release marked Latest. Publish it later with
#   gh release edit vVERSION -R OWNER/REPO --draft=false --latest
# With --publish it is public at once and marked Latest, so every
# `curl … | sh` and `ibara update` gets it from then on.
#
# KEY defaults to ~/.config/ibara-release/ibara-release and the private-markers
# file to ~/.config/ibara-release/private-markers. The other options are
# release.sh's (see there). Needs gh signed in with access to the repository.
set -euo pipefail

usage() {
  echo "Usage: packaging/publish.sh [--publish] [--sign-key KEY] [--private-markers FILE] [--out DIR] [--built VERIFIED_DIR] --plugin DIR --stream DIR --view DIR --notes FILE VERSION" >&2
  exit 64
}
built=''
publish=0 key=${XDG_CONFIG_HOME:-$HOME/.config}/ibara-release/ibara-release out=''
markers=${XDG_CONFIG_HOME:-$HOME/.config}/ibara-release/private-markers plugin='' stream='' view='' notes='' version=''
while (($#)); do
  case $1 in
    --publish) publish=1; shift ;;
    --built) built=${2:?}; shift 2 ;;
    --sign-key) key=${2:?}; shift 2 ;;
    --private-markers) markers=${2:?}; shift 2 ;;
    --out) out=${2:?}; out_override=1; shift 2 ;;
    --plugin) plugin=${2:?}; shift 2 ;;
    --stream) stream=${2:?}; shift 2 ;;
    --view) view=${2:?}; shift 2 ;;
    --notes) notes=${2:?}; shift 2 ;;
    -*) usage ;;
    *) [[ -z $version ]] || usage; version=$1; shift ;;
  esac
done
[[ -n $plugin && -n $stream && -n $view && -n $notes && -n $version ]] || usage

core=$(cd -- "$(dirname -- "$(realpath -- "${BASH_SOURCE[0]}")")/.." && pwd -P)
fail() { echo "publish: $*" >&2; exit 1; }

# shellcheck source=release.env
. "$core/packaging/release.env"
[[ ${IBARA_BASE_URL:-} =~ ^https://github\.com/([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)/releases/latest/download$ ]] ||
  fail 'IBARA_BASE_URL in packaging/release.env is not https://github.com/OWNER/REPO/releases/latest/download.'
repo=${BASH_REMATCH[1]}
[[ $version =~ ^([0-9][0-9A-Za-z.]*)-([1-9][0-9]*)$ ]] || fail "VERSION must be PKGVER-PKGREL, for example 0.1.0-1."
pkgver=${BASH_REMATCH[1]} pkgrel=${BASH_REMATCH[2]}
[[ $pkgver == "$(sed -n 's/^pkgver=//p' "$core/packaging/PKGBUILD")" ]] ||
  fail "packaging/PKGBUILD's pkgver is $(sed -n 's/^pkgver=//p' "$core/packaging/PKGBUILD"), not $pkgver."
tag=v$version
[[ -r $key ]] || fail "No signing key at $key (see --sign-key)."
[[ -f $markers ]] || fail "No private-markers file at $markers (see --private-markers and release.sh)."
[[ -s $notes ]] || fail 'The notes file is empty.'

# Everything GitHub must already have, checked before the long build.
command -v gh >/dev/null || fail 'gh is missing: sudo pacman -S github-cli'
gh auth status >/dev/null 2>&1 || fail 'gh is not signed in: gh auth login'
gh repo view "$repo" --json name >/dev/null 2>&1 || fail "gh cannot see the repository $repo."
commit=$(git -C "$core" rev-parse HEAD)
gh api "repos/$repo/commits/$commit" --silent 2>/dev/null ||
  fail "$repo does not have this checkout's commit $commit yet: push it first."
# Drafts included: gh release list names them, the tags API does not.
if gh release list -R "$repo" --limit 1000 --json tagName --jq '.[].tagName' | grep -qxF "$tag"; then
  fail "$repo already has a release $tag."
fi
if existing=$(gh api "repos/$repo/git/ref/tags/$tag" --jq .object.sha 2>/dev/null); then
  [[ $existing == "$commit" ]] || fail "$repo already has a tag $tag, at $existing, not this checkout's $commit."
fi

out=${out:-${XDG_CACHE_HOME:-$HOME/.cache}/ibara-release/$version}
if [[ -n $built ]]; then
  [[ -z ${out_override:-} ]] || fail "Use --built without --out."
  out=$built
else
if [[ -e $out ]]; then
  [[ -d $out && -z $(ls -A "$out") ]] || fail "$out already holds files; remove it or name another with --out."
fi
IBARA_PKGREL=$pkgrel "$core/packaging/release.sh" --plugin "$plugin" --stream "$stream" --view "$view" \
  --sign-key "$key" --notes "$notes" --private-markers "$markers" "$out"
fi
out=$(cd -- "$out" && pwd -P)

# The files of the release, exactly: GitHub keeps names made of these characters as they are.
packages=("ibara-$version-x86_64.pkg.tar.zst" "ibara-stream-$version-x86_64.pkg.tar.zst" "ibara-view-$version-x86_64.pkg.tar.zst")
assets=("${packages[@]}" "ibara-$version-source.tar.gz" install stable.json stable.json.sig)
[[ $(ls -A "$out" | sort) == "$(printf '%s\n' "${assets[@]}" | sort)" ]] ||
  fail "$out does not hold exactly the release's files: $(ls -A "$out" | tr '\n' ' ')"
for asset in "${assets[@]}"; do
  [[ $asset =~ ^[A-Za-z0-9._-]+$ ]] || fail "GitHub would rename $asset."
done

check=$(mktemp -d)
trap 'rm -rf "$check"' EXIT
printf 'ibara-release namespaces="ibara-release" %s\n' "$IBARA_RELEASE_KEY" >"$check/allowed_signers"
ssh-keygen -Y verify -f "$check/allowed_signers" -I ibara-release -n ibara-release \
  -s "$out/stable.json.sig" <"$out/stable.json" >/dev/null 2>&1 || fail 'stable.json.sig does not verify with IBARA_RELEASE_KEY.'
jq -e --arg v "$version" '.schema_version == 2 and .name == "ibara" and .version == $v' "$out/stable.json" >/dev/null ||
  fail "stable.json is not release $version."
jq -e --arg f "ibara-$version-source.tar.gz" '.source.file == $f' "$out/stable.json" >/dev/null ||
  fail 'stable.json names another source tarball.'
[[ $(jq -r .source.core "$out/stable.json") == "$commit" ]] || fail 'stable.json names another core commit.'
for file in "${packages[@]}"; do
  name=${file%-"$version"-x86_64.pkg.tar.zst}
  jq -e --arg n "$name" --arg f "$file" --arg sha "$(sha256sum "$out/$file" | cut -d' ' -f1)" \
    --argjson size "$(stat -c %s "$out/$file")" \
    '[.packages[] | select(.name == $n)] == [{name: $n, file: $f, sha256: $sha, size: $size}]' "$out/stable.json" >/dev/null ||
    fail "stable.json does not match $file."
  bsdtar -xOf "$out/$file" .PKGINFO >"$check/PKGINFO"
  grep -qxF "pkgname = $name" "$check/PKGINFO" && grep -qxF "pkgver = $version" "$check/PKGINFO" ||
    fail "$file is not $name $version."
done
# The channel `ibara update` uses is the one built into the package's own ibara.
bsdtar -xOf "$out/${packages[0]}" usr/lib/ibara/bin/ibara >"$check/ibara"
grep -aqF "IBARA_BASE_URL=\"$IBARA_BASE_URL\"" "$check/ibara" && grep -aqF "$IBARA_RELEASE_KEY" "$check/ibara" ||
  fail "The packaged ibara was not built with this release.env's channel."
grep -qxF "IBARA_BASE_URL='$IBARA_BASE_URL'" "$out/install" && grep -qxF "IBARA_RELEASE_KEY='$IBARA_RELEASE_KEY'" "$out/install" ||
  fail 'install does not name this channel.'
sh -n "$out/install"
echo "Release $version in $out is complete and signed." >&2

{
  jq -r '.notes[] | "- " + .' "$out/stable.json"
  printf '\n## Install\n\nOn an Omarchy computer:\n\n```bash\ncurl -fsSL %s/install | sh\n```\n\n' "$IBARA_BASE_URL"
  echo 'Already installed? Run `ibara update`.'
  printf '\n`ibara-%s-source.tar.gz` is the full source of the three packages. Core and the Omarchy plugin are MIT; the separate ibara-stream (Sunshine) and ibara-view (Moonlight) programs remain GPL-3.0.\n' "$version"
} >"$check/body.md"

if ((publish)); then
  state=(--latest)
else
  state=(--draft)
fi
gh release create "$tag" -R "$repo" --target "$commit" --title "ibara $version" --notes-file "$check/body.md" \
  "${state[@]}" "${assets[@]/#/$out/}"

# What GitHub holds is what was checked here.
gh release view "$tag" -R "$repo" --json assets --jq '.assets[] | [.name, (.size | tostring), .digest] | join(" ")' |
  sort >"$check/uploaded"
for asset in "${assets[@]}"; do
  printf '%s %s sha256:%s\n' "$asset" "$(stat -c %s "$out/$asset")" "$(sha256sum "$out/$asset" | cut -d' ' -f1)"
done | sort >"$check/local"
diff "$check/local" "$check/uploaded" >&2 || fail "The files on GitHub differ from $out (above). The release $tag is there; delete it with: gh release delete $tag -R $repo --cleanup-tag"

url=$(gh release view "$tag" -R "$repo" --json url --jq .url)
if ((publish)); then
  echo "Published ibara $version: $url"
  echo "Install: curl -fsSL $IBARA_BASE_URL/install | sh"
else
  echo "ibara $version is a draft: $url"
  echo "Publish it: gh release edit $tag -R $repo --draft=false --latest"
fi
