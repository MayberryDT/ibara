#!/usr/bin/env bash
# Build and sign one ibara release into OUT. Nothing is published:
# packaging/publish.sh runs this, then uploads OUT as a GitHub release.
#
#   packaging/release.sh --plugin PLUGIN_CHECKOUT --stream IBARA_STREAM_CHECKOUT \
#     --view IBARA_VIEW_CHECKOUT --sign-key PRIVATE_KEY --notes NOTES_FILE \
#     --private-markers MARKERS_FILE OUT
#
# OUT receives:
#   install                                 the one-line installer (curl -fsSL URL/install | sh)
#   stable.json, stable.json.sig            the release manifest and its signature
#   ibara-VERSION-x86_64.pkg.tar.zst        ibara, ibarad and workspace ibara-screen
#   ibara-stream-VERSION-x86_64.pkg.tar.zst Take Control of this computer (the ibara-stream fork, branch ibara)
#   ibara-view-VERSION-x86_64.pkg.tar.zst   the viewer (the ibara-view fork, branch ibara)
#   ibara-VERSION-source.tar.gz             the sources all three were built from
#
# All three packages have the release's version: ibara depends on exactly those
# ibara-stream and ibara-view, and installing, updating and going back move
# them together.
#
# Needs: makepkg, cargo, and the build tools of both forks (their
# packaging/ibara/PKGBUILD); clean checkouts, the forks with their submodules;
# packaging/release.env filled in; PRIVATE_KEY the private half of
# IBARA_RELEASE_KEY. When ssh-agent (SSH_AUTH_SOCK) holds that key, it signs and
# the passphrase is never asked; otherwise ssh-keygen asks for it twice (the
# check below and the signature). NOTES_FILE has one plain sentence per line; the console
# shows them once after the update ("What's New"). The release number is
# IBARA_PKGREL (default 1). makepkg's MAKEFLAGS and SRCDEST pass through; the
# packages are built in a folder under /var/tmp, whatever BUILDDIR says, so
# makepkg's .BUILDINFO names no folder of the person building.
#
# MARKERS_FILE lists what must never be published, one string per line
# (addresses, host names, account names, identifiers; blank lines and lines
# starting with # are skipped). It is kept outside every repository, since
# listing the strings would publish them. The release is refused when the
# tree of core, the plugin or either fork holds one of them, in a file or a
# file name, compared without regard to case: those trees become the source
# tarball, and the commit is what gets pushed. The forks' upstream submodules
# are not searched. It is refused again when a built package holds one, in any
# file it installs, its .PKGINFO or .BUILDINFO, or a file name: build paths
# reach programs through panic locations and __FILE__.
set -euo pipefail

usage() {
  echo "Usage: packaging/release.sh --plugin DIR --stream DIR --view DIR --sign-key KEY --notes FILE --private-markers FILE OUT" >&2
  exit 64
}
plugin='' stream='' view='' key='' notes='' markers='' out=''
while (($#)); do
  case $1 in
    --plugin) plugin=${2:?}; shift 2 ;;
    --stream) stream=${2:?}; shift 2 ;;
    --view) view=${2:?}; shift 2 ;;
    --sign-key) key=${2:?}; shift 2 ;;
    --notes) notes=${2:?}; shift 2 ;;
    --private-markers) markers=${2:?}; shift 2 ;;
    -*) usage ;;
    *) [[ -z $out ]] || usage; out=$1; shift ;;
  esac
done
[[ -n $plugin && -n $stream && -n $view && -n $key && -n $notes && -n $markers && -n $out ]] || usage

core=$(cd -- "$(dirname -- "$(realpath -- "${BASH_SOURCE[0]}")")/.." && pwd -P)
plugin=$(cd -- "$plugin" && pwd -P)
stream=$(cd -- "$stream" && pwd -P)
view=$(cd -- "$view" && pwd -P)
fail() { echo "release: $*" >&2; exit 1; }

# shellcheck source=release.env
. "$core/packaging/release.env"
[[ -n ${IBARA_BASE_URL:-} && -n ${IBARA_RELEASE_KEY:-} ]] || fail 'Set IBARA_BASE_URL and IBARA_RELEASE_KEY in packaging/release.env first.'
[[ $IBARA_BASE_URL != */ ]] || fail 'IBARA_BASE_URL must not end with a slash.'
pub=$(cut -d' ' -f1-2 <<<"$IBARA_RELEASE_KEY")
if ssh-add -L 2>/dev/null | cut -d' ' -f1-2 | grep -qxF "$pub"; then
  in_agent=1
else
  in_agent=
  [[ $(ssh-keygen -y -f "$key" | cut -d' ' -f1-2) == "$pub" ]] || fail 'The signing key is not the private half of IBARA_RELEASE_KEY.'
fi
for repo in "$core" "$plugin" "$stream" "$view"; do
  # Submodules checked out at another commit show here too.
  [[ -z $(git -C "$repo" status --porcelain --untracked-files=no) ]] || fail "$repo has uncommitted changes."
done
for fork in "$stream" "$view"; do
  [[ -f $fork/packaging/ibara/PKGBUILD ]] || fail "$fork has no packaging/ibara/PKGBUILD; check out its ibara branch."
done
[[ -f $markers ]] || fail "No private-markers file at $markers: list what must never be published, one string per line."
patterns=$(mktemp)
trap 'rm -f "$patterns"' EXIT
grep -v -e '^[[:space:]]*$' -e '^#' "$markers" >"$patterns" || true
for repo in "$core" "$plugin" "$stream" "$view"; do
  [[ -s $patterns ]] || break
  found=$({
    git -C "$repo" grep -a -i -F -l -f "$patterns" HEAD -- || true
    git -C "$repo" ls-tree -r --name-only HEAD | grep -i -F -f "$patterns" | sed 's/^/HEAD:(name) /' || true
  } | head -n 20)
  [[ -z $found ]] || { rm -f "$patterns"; fail "$repo holds strings from $markers, so it must not be published. Files:"$'\n'"$found"; }
done
grep -qF "readonly property string ibaraBaseUrl: \"$IBARA_BASE_URL\"" "$plugin/Service.qml" ||
  fail "Set ibaraBaseUrl in the plugin's Service.qml to $IBARA_BASE_URL (the marketplace copy shows the install command from it)."
[[ -s $notes ]] || fail 'The notes file is empty.'
pkgver=$(sed -n 's/^pkgver=//p' "$core/packaging/PKGBUILD")
pkgrel=${IBARA_PKGREL:-1}
version=$pkgver-$pkgrel
mkdir -p "$out"
out=$(cd -- "$out" && pwd -P)

build=$(mktemp -d /var/tmp/ibara-release.XXXXXXXX)
trap 'rm -rf "$build" "$patterns"' EXIT
# build_package NAME PKGBUILD_DIR VAR=VALUE…: one package into $build. What it
# installs must hold no marker either: every file, .PKGINFO, .BUILDINFO and
# every file name.
build_package() {
  local name=$1 dir=$2 file found
  shift 2
  mkdir -p "$build/$name"
  cp "$dir"/PKGBUILD "$build/$name/"
  [[ ! -f $dir/$name.install ]] || cp "$dir/$name.install" "$build/$name/"
  echo "Building $name $version…" >&2
  (cd "$build/$name" && env "$@" IBARA_PKGREL="$pkgrel" PKGDEST="$build" BUILDDIR="$build" makepkg --force --nodeps --noconfirm >&2)
  file=$build/$name-$version-x86_64.pkg.tar.zst
  [[ -f $file ]] || fail "makepkg made no $name-$version-x86_64.pkg.tar.zst."
  [[ -s $patterns ]] || return 0
  mkdir -p "$build/scan/$name"
  bsdtar -xf "$file" -C "$build/scan/$name"
  found=$({
    grep -r -a -i -F -l -f "$patterns" "$build/scan/$name" | sed "s|^$build/scan/||" || true
    bsdtar -tf "$file" | grep -i -F -f "$patterns" | sed "s|^|$name:(name) |" || true
  } | head -n 20)
  rm -rf "$build/scan"
  [[ -z $found ]] || fail "The built $name package holds strings from $markers, so it must not be published. Files:"$'\n'"$found"
}
build_package ibara "$core/packaging" IBARA_CORE_DIR="$core" IBARA_PLUGIN_DIR="$plugin"
# The own-screen workspace binary must ship with core; retain both fallback packages.
mkdir -p "$build/verify-core"
bsdtar -xf "$build/ibara-$version-x86_64.pkg.tar.zst" -C "$build/verify-core" usr/lib/ibara/bin/ibara-screen
[[ -x $build/verify-core/usr/lib/ibara/bin/ibara-screen ]] || fail 'The ibara package is missing ibara-screen.'
rm -rf "$build/verify-core"
build_package ibara-stream "$stream/packaging/ibara" IBARA_STREAM_DIR="$stream" IBARA_VERSION="$pkgver"
build_package ibara-view "$view/packaging/ibara" IBARA_VIEW_DIR="$view" IBARA_VERSION="$pkgver"

# The sources the packages were built from: core (MIT), the plugin (MIT)
# and both forks (GPL-3.0) with their submodules. Two submodules hold only
# prebuilt binaries and are named by commit instead: ibara-stream's
# third-party/build-deps (the FFmpeg it links comes from its release of that
# tag) and ibara-view's libs (Windows and macOS libraries).
append() { # REPO PREFIX
  git -C "$1" archive --format=tar --prefix="$2/" HEAD >"$build/part.tar"
  tar --concatenate --file="$build/source.tar" "$build/part.tar"
}
# submodules NAME REPO [PATH]: the checked-out submodules of REPO, recursively,
# as paths from the fork's top, not entering the prebuilt ones.
submodules() {
  local name=$1 repo=$2 top=${3:-} path sub
  git -C "$repo" config -f .gitmodules --get-regexp '\.path$' 2>/dev/null | while read -r _ path; do
    sub=${top:+$top/}$path
    case $name/$sub in
      ibara-stream/third-party/build-deps | ibara-view/libs) continue ;;
    esac
    [[ -e $repo/$path/.git ]] || continue
    echo "$sub"
    submodules "$name" "$repo/$path" "$sub"
  done
}
tar --create --file="$build/source.tar" --files-from=/dev/null
append "$core" "ibara-$version/core"
append "$plugin" "ibara-$version/omarchy-ibara"
declare -A forks=([ibara-stream]=$stream [ibara-view]=$view)
for name in ibara-stream ibara-view; do
  fork=${forks[$name]}
  append "$fork" "ibara-$version/$name"
  while read -r sub; do
    append "$fork/$sub" "ibara-$version/$name/$sub"
  done < <(submodules "$name" "$fork")
  mkdir -p "$build/notes/ibara-$version/$name"
  git -C "$fork" submodule status --recursive >"$build/notes/ibara-$version/$name/SUBMODULES"
done
tar --append --file="$build/source.tar" -C "$build/notes" "ibara-$version"
gzip -9n <"$build/source.tar" >"$out/ibara-$version-source.tar.gz"

packages='[]'
for name in ibara ibara-stream ibara-view; do
  file=$name-$version-x86_64.pkg.tar.zst
  cp "$build/$file" "$out/$file"
  packages=$(jq --arg name "$name" --arg file "$file" --arg sha "$(sha256sum "$out/$file" | cut -d' ' -f1)" \
    --argjson size "$(stat -c %s "$out/$file")" '. + [{name: $name, file: $file, sha256: $sha, size: $size}]' <<<"$packages")
done
# Carry only notes from the previously verified channel into the new signed
# manifest. An offline build can still produce a release with its own notes.
history='[]'
printf 'ibara-release namespaces="ibara-release" %s\n' "$IBARA_RELEASE_KEY" >"$build/allowed_signers"
if curl -fsSL --proto '=https' --proto-redir '=https' --max-time 12 --max-filesize 262144 "$IBARA_BASE_URL/stable.json" -o "$build/previous.json" &&
   curl -fsSL --proto '=https' --proto-redir '=https' --max-time 12 --max-filesize 262144 "$IBARA_BASE_URL/stable.json.sig" -o "$build/previous.json.sig" &&
   ssh-keygen -Y verify -f "$build/allowed_signers" -I ibara-release -n ibara-release -s "$build/previous.json.sig" <"$build/previous.json" >/dev/null 2>&1; then
  history=$(jq --arg version "$version" '([{version, released_at, notes}] + (.history // [])) | map(select(.version != $version)) | reduce .[] as $r ([]; if any(.[]; .version == $r.version) then . else . + [$r] end) | .[:9]' "$build/previous.json")
fi
jq -n --arg version "$version" --argjson packages "$packages" --argjson history "$history" --arg at "$(date -u +%FT%TZ)" \
  --arg core "$(git -C "$core" rev-parse HEAD)" --arg plugin "$(git -C "$plugin" rev-parse HEAD)" \
  --arg stream "$(git -C "$stream" rev-parse HEAD)" --arg view "$(git -C "$view" rev-parse HEAD)" \
  --rawfile notes "$notes" \
  '{schema_version: 2, name: "ibara", version: $version, released_at: $at, packages: $packages,
    source: {core: $core, plugin: $plugin, stream: $stream, view: $view, file: ("ibara-" + $version + "-source.tar.gz")},
    notes: ($notes | split("\n") | map(select(length > 0))), history: $history}' >"$out/stable.json"
rm -f "$out/stable.json.sig"
if [[ -n $in_agent ]]; then
  # With a public key, ssh-keygen signs through ssh-agent.
  printf '%s\n' "$IBARA_RELEASE_KEY" >"$build/release-key.pub"
  ssh-keygen -q -Y sign -f "$build/release-key.pub" -n ibara-release "$out/stable.json"
else
  ssh-keygen -q -Y sign -f "$key" -n ibara-release "$out/stable.json"
fi
printf 'ibara-release namespaces="ibara-release" %s\n' "$IBARA_RELEASE_KEY" >"$build/allowed_signers"
ssh-keygen -Y verify -f "$build/allowed_signers" -I ibara-release -n ibara-release -s "$out/stable.json.sig" <"$out/stable.json" >/dev/null

sed -e "s|@IBARA_BASE_URL@|$IBARA_BASE_URL|g" -e "s|@IBARA_RELEASE_KEY@|$IBARA_RELEASE_KEY|g" "$core/packaging/install.sh" >"$out/install"
sh -n "$out/install"
echo "Release $version is in $out (packaging/publish.sh uploads it). The install command is:"
echo "  curl -fsSL $IBARA_BASE_URL/install | sh"
