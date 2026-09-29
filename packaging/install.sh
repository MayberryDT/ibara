#!/bin/sh
# ibara's one-line installer:
#
#   curl -fsSL @IBARA_BASE_URL@/install | sh
#
# Downloads the newest release, checks its signature and the digest of each of
# its three packages (ibara, and ibara-stream and ibara-view for Take Control),
# installs them together with pacman, then runs `ibara setup`, which finishes
# setting up this computer (it asks for your password once).
# packaging/release.sh fills in the two values below from
# packaging/release.env; this template is never published as it is.
#
# Everything runs inside main(), so a download cut short runs nothing.

IBARA_BASE_URL='@IBARA_BASE_URL@'
IBARA_RELEASE_KEY='@IBARA_RELEASE_KEY@'

say() { printf '%s\n' "$*"; }
die() { printf '\nibara was not installed: %s\n' "$*" >&2; exit 1; }

fetch() {
  case $IBARA_BASE_URL in
    https://*) proto='=https' ;;
    *) proto='=http' ;;
  esac
  curl -fsSL --proto "$proto" --proto-redir "$proto" --connect-timeout 15 -o "$2" "$IBARA_BASE_URL/$1" ||
    die "could not download $IBARA_BASE_URL/$1. Check your connection and try again."
}

main() {
  set -eu
  [ "$(id -u)" -ne 0 ] || die "run it as yourself, not as root. It asks for your password when it needs it."
  command -v pacman >/dev/null 2>&1 || die "ibara needs Arch Linux with Omarchy."
  for tool in curl jq sha256sum sudo; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is missing. Install it with: sudo pacman -S $tool"
  done
  if ! command -v omarchy >/dev/null 2>&1 && [ ! -d /usr/share/omarchy ]; then
    say "Omarchy was not found. ibara installs, but its bar icon and console need Omarchy."
  fi
  if ! command -v ssh-keygen >/dev/null 2>&1; then
    say "Installing OpenSSH first, to check the release signature (sudo)."
    sudo pacman -S --needed --noconfirm openssh || die "OpenSSH did not install."
  fi

  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT INT TERM

  say "Checking the newest ibara release…"
  fetch stable.json "$tmp/stable.json"
  fetch stable.json.sig "$tmp/stable.json.sig"
  printf 'ibara-release namespaces="ibara-release" %s\n' "$IBARA_RELEASE_KEY" >"$tmp/allowed_signers"
  ssh-keygen -Y verify -f "$tmp/allowed_signers" -I ibara-release -n ibara-release \
    -s "$tmp/stable.json.sig" <"$tmp/stable.json" >/dev/null 2>&1 ||
    die "the release is not signed with ibara's key."

  version=$(jq -r '.version // empty' "$tmp/stable.json")
  [ -n "$version" ] || die "the release is incomplete."
  set --
  for name in ibara ibara-stream ibara-view; do
    file=$(jq -r --arg name "$name" '[.packages[]? | select(.name == $name)] | if length == 1 then .[0].file // empty else empty end' "$tmp/stable.json")
    sha=$(jq -r --arg name "$name" '[.packages[]? | select(.name == $name)] | if length == 1 then .[0].sha256 // empty else empty end' "$tmp/stable.json")
    # This package at this version, and nothing that could be a path.
    [ "$file" = "$name-$version-x86_64.pkg.tar.zst" ] || die "the release names no $name package."
    case $file in
      */* | *..*) die "the release names no $name package." ;;
    esac
    [ ${#sha} -eq 64 ] || die "the release is incomplete."

    say "Downloading $name $version…"
    fetch "$file" "$tmp/$file"
    printf '%s  %s\n' "$sha" "$tmp/$file" | sha256sum -c --quiet - >/dev/null 2>&1 ||
      die "the $name download does not match the signed release."
    set -- "$@" "$tmp/$file"
  done

  say "Installing ibara $version. This needs your password (sudo)."
  sudo pacman -U --needed --noconfirm "$@" ||
    die "pacman could not install it. If a package failed to download, run: omarchy update, then try again."
  # Kept for `ibara rollback`, as `ibara update` keeps every release it installs.
  sudo install -D -m 0644 -t /var/cache/ibara/packages "$@"

  say ""
  # The package's own ibara: an earlier hand installation may still have one
  # earlier in PATH (~/.local/bin) until setup replaces it.
  if (: </dev/tty) 2>/dev/null; then
    /usr/bin/ibara setup </dev/tty
  else
    /usr/bin/ibara setup
  fi
}

main "$@"
