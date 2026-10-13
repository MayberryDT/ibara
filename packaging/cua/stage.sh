#!/usr/bin/env bash
# Stage the exact qualified MIT Cua Driver bundle; never installs its optional helpers.
set -euo pipefail
out=${1:?destination}
cache=${2:?build cache}
version=0.34.1
sha=a41fe2bdcbeea4501dc594c074f16b0975728e0cc7b08b90acea8564d48fafee
url=https://github.com/trycua/cua/releases/download/cua-driver-rs-v0.34.1/cua-driver-rs-0.34.1-linux-x86_64-binary.tar.gz
mkdir -p "$cache" "$out"
archive=${IBARA_CUA_ARCHIVE:-$cache/cua-driver-0.34.1.tar.gz}
if [[ ! -f $archive ]];then
 curl -fsSL --proto '=https' --proto-redir '=https' --connect-timeout 15 --max-time 180 --max-filesize 536870912 "$url" -o "$archive.partial"
 printf '%s  %s\n' "$sha" "$archive.partial" | sha256sum -c - >/dev/null
 mv "$archive.partial" "$archive"
fi
printf '%s  %s\n' "$sha" "$archive" | sha256sum -c - >/dev/null
stage=$(mktemp -d "$cache/cua-stage.XXXXXXXX")
trap 'rm -rf "$stage"' EXIT
tar -xzf "$archive" --no-same-owner -C "$stage" cua-driver cua-cursor-theme libcua_driver_sdk.so cua_driver_node_runtime.node cua_driver_abi.h
for file in cua-driver cua-cursor-theme;do install -m755 "$stage/$file" "$out/$file";done
for file in libcua_driver_sdk.so cua_driver_node_runtime.node cua_driver_abi.h;do install -m644 "$stage/$file" "$out/$file";done
install -m644 "$(dirname "$0")/LICENSE" "$out/LICENSE"
printf '{"version":"%s","archive_sha256":"%s","source_commit":"0c69d9a2c8abbb5051a2df4a23f0fa166d5385dc","url":"%s"}\n' "$version" "$sha" "$url" > "$out/provenance.json"
