#!/usr/bin/env bash
# Build an operator-only Ubuntu package. Shipping publication requires qualification.
# Set IBARA_PKGREL for a release-profile package; otherwise use a private build ID.
set -euo pipefail
core=$(cd -- "$(dirname -- "$0")/../.." && pwd)
: "${IBARA_BUILD_ROOT:?Set the build output directory}"
: "${IBARA_PLUGIN_SOURCE:?Set the Console source directory}"
: "${IBARA_QUICKSHELL_ROOT:?Set the staged Quickshell directory}"
: "${IBARA_QUICKSHELL_SOURCE:?Set the Quickshell source directory}"
build=$IBARA_BUILD_ROOT
plugin=$IBARA_PLUGIN_SOURCE
export CARGO_TARGET_DIR="$build/target"
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}
profile=debug
flags=()
if [[ -n ${IBARA_PKGREL:-} ]]; then
  profile=release
  flags=(--release)
  export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$core=/usr/src/ibara --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/usr/src/cargo"
  stage_id="release-$IBARA_PKGREL"
else
  : "${IBARA_BUILD_ID:?Choose a fresh private build ID}"
  stage_id=$IBARA_BUILD_ID
fi
export IBARA_BUILD_ID
cargo build "${flags[@]}" --locked --manifest-path "$core/Cargo.toml" -p ibara --bins
# Always rebuild without private qualification entry points before packaging.
CARGO_TARGET_DIR="$build/screen-target" cargo build "${flags[@]}" --locked --manifest-path "$core/Cargo.toml" -p ibara-screen --no-default-features
version=$("$CARGO_TARGET_DIR/$profile/ibara" --version)
version=${version#ibara }
asset_name=ibara
if [[ $profile == release ]]; then
  stage_id="release-$version"
  asset_name=ibara-operator
fi
stage="$build/deb-operator-root-$stage_id"
[[ ! -e $stage ]] || { echo "Existing stage: $stage; preserve or move it before rebuilding." >&2; exit 1; }
install -d "$stage/DEBIAN" "$stage/usr/lib/ibara/bin" "$stage/usr/bin" \
  "$stage/usr/lib/systemd/user" "$stage/usr/share/applications" \
  "$stage/usr/share/icons/hicolor/scalable/apps" "$stage/usr/share/doc/ibara"
install -m755 "$CARGO_TARGET_DIR/$profile/ibara" "$CARGO_TARGET_DIR/$profile/ibarad" "$stage/usr/lib/ibara/bin/"
ln -s ../lib/ibara/bin/ibara "$stage/usr/bin/ibara"
ln -s ../lib/ibara/bin/ibarad "$stage/usr/bin/ibarad"
# The active portable viewer is shared with the existing screen protocol.
# GNOME mechanisms are proven separately; operator setup grants no target access.
install -m755 "$build/screen-target/$profile/ibara-screen" "$stage/usr/lib/ibara/bin/ibara-screen"
ln -s ../lib/ibara/bin/ibara-screen "$stage/usr/bin/ibara-screen"
cp -a "$IBARA_QUICKSHELL_ROOT/usr/lib/ibara/quickshell" "$stage/usr/lib/ibara/"
python3 "$plugin/standalone/stage.py" "$stage/usr/share/ibara/console"
printf '{"version":"%s"}\n' "$version" > "$stage/usr/share/ibara/console/plugin/build.json"
install -m755 "$core/packaging/deb/ibara-console" "$stage/usr/bin/ibara-console"
install -m644 "$core/packaging/systemd/ibara-operator.service" "$stage/usr/lib/systemd/user/"
install -m644 "$core/packaging/deb/io.zet.ibara.desktop" "$stage/usr/share/applications/"
install -m644 "$core/packaging/icons/io.zet.ibara.svg" "$stage/usr/share/icons/hicolor/scalable/apps/"
install -m644 "$core/LICENSE" "$stage/usr/share/doc/ibara/copyright"
install -m644 "$IBARA_QUICKSHELL_SOURCE/LICENSE" "$stage/usr/share/doc/ibara/quickshell-LICENSE"
install -m644 "$plugin/LICENSE" "$stage/usr/share/doc/ibara/console-LICENSE"
cat > "$stage/DEBIAN/control" <<CONTROL
Package: ibara
Version: $version
Architecture: amd64
Maintainer: Tyler Mayberry
Depends: libc6 (>= 2.39), libgcc-s1, libstdc++6, libjemalloc2, libqt6core6t64 (>= 6.10), libqt6core6t64 (<< 6.11), libqt6gui6 (>= 6.10), libqt6qml6 (>= 6.10), libqt6quick6 (>= 6.10), libqt6widgets6 (>= 6.10), libqt6network6 (>= 6.10), libqt6dbus6 (>= 6.10), qt6-wayland, qml6-module-qtquick, qml6-module-qtquick-window, qml6-module-qtquick-controls, qml6-module-qtquick-shapes, qml6-module-qtmultimedia, libqt6svg6, qt6-image-formats-plugins, openssh-client, openssl, curl, libnotify-bin, zenity, libva2, libva-drm2, libdrm2, libgbm1, libwayland-client0, libxkbcommon0, libpipewire-0.3-0t64
Description: ibara operator Console for Ubuntu 26.04
 Use paired computers through the standalone Console without an Omarchy shell.
 This operator-only package grants no target input or streaming access.
CONTROL
cat > "$stage/DEBIAN/preinst" <<'PREINST'
#!/bin/sh
set -eu
. /etc/os-release
[ "$ID" = ubuntu ] && [ "$VERSION_ID" = 26.04 ] || {
  echo 'This package is for Ubuntu 26.04 only.' >&2
  exit 1
}
PREINST
chmod 755 "$stage/DEBIAN/preinst"
install -d "$build/packages"
dpkg-deb --threads-max=2 -Zzstd -z3 --root-owner-group --build "$stage" "$build/packages/${asset_name}_${version}_amd64.deb"
sha256sum "$build/packages/${asset_name}_${version}_amd64.deb"
