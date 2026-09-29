#!/usr/bin/env bash
# A clean, throwaway Omarchy-like Arch container for installing ibara end to
# end, on any Arch host with systemd-nspawn and sudo. Nothing on the host
# changes except the paths below; the container has its own network namespace
# (loopback only), so its firewall and services never touch the host's.
#
#   container.sh create BOOTSTRAP_TARBALL   root filesystem from Arch's bootstrap tarball,
#                                           Omarchy's package mirror and keys, ibara's
#                                           dependencies, a desktop account `alice`
#                                           (passwordless sudo) and stand-ins for
#                                           Omarchy's shell commands
#   container.sh boot                       start it (transient unit ibara-e2e.service)
#   container.sh channel DIR                serve release DIR inside it at the address its
#                                           install names: http://127.0.0.1:8080, or
#                                           https://github.com/OWNER/REPO/releases/latest/download
#                                           laid out and redirected as GitHub does (release-server,
#                                           with a test CA trusted only in the container); DIR
#                                           becomes the Latest release, earlier ones stay
#   container.sh alice 'COMMAND'            run COMMAND in a login session of alice
#   container.sh root 'COMMAND'             run COMMAND as root inside it
#   container.sh snapshot | reset           keep the fresh root filesystem; go back to it (stopped)
#   container.sh stop | destroy             stop it; remove it, its snapshot and its unit
#
# Host paths: /var/lib/machines/ibara-e2e (root filesystem) and ibara-e2e.fresh (its
# snapshot), unit ibara-e2e.service.
set -euo pipefail
machine=ibara-e2e
root=/var/lib/machines/$machine
here=$(cd -- "$(dirname -- "$(realpath -- "${BASH_SOURCE[0]}")")" && pwd -P)

# The packages' dependencies (packaging/PKGBUILD and the ibara-stream and
# ibara-view PKGBUILDs of the forks), installed while the container still
# shares the host's network, so installing the release later needs none. Plus
# what an Omarchy computer already has: ufw, jq, sudo, git; and sway, a
# compositor that runs without a screen, for streaming a desktop in the container.
deps=(hyprland grim at-spi2-core cua-driver-bin openssh openssl acl sudo curl pacman tailscale
  cmake ninja gcc pkgconf mousepad foot ufw jq git python sqlite sway
  glib2 icu libcap libdrm libevdev libpulse libquadmath libva libx11 mesa numactl opus wayland
  ffmpeg libglvnd libplacebo qt6-base qt6-declarative qt6-svg sdl2-compat libgcc libstdc++)

offline() { sudo systemd-nspawn -q -D "$root" --pipe "$@"; }
inside() { sudo systemd-run -M "$machine" --quiet --wait --pipe --collect "$@"; }

case ${1:-} in
  create)
    tarball=${2:?bootstrap tarball}
    [[ ! -e $root ]] || { echo "$root exists; destroy it first." >&2; exit 1; }
    work=$(sudo mktemp -d /var/lib/machines/.ibara-e2e-XXXXXX)
    sudo bsdtar -C "$work" -xpf "$tarball"
    sudo mv "$work/root.x86_64" "$root"
    sudo rm -rf --one-file-system "$work"
    # Omarchy's own package sources and keys, so the container gets the same
    # Hyprland, Tailscale and cua-driver-bin as an Omarchy computer.
    sudo cp /etc/pacman.conf "$root/etc/pacman.conf"
    sudo cp /etc/pacman.d/mirrorlist "$root/etc/pacman.d/mirrorlist"
    sudo cp /usr/share/pacman/keyrings/omarchy* "$root/usr/share/pacman/keyrings/"
    offline pacman-key --init >/dev/null
    offline pacman-key --populate archlinux omarchy >/dev/null
    offline pacman -Syu --noconfirm --needed base "${deps[@]}"
    echo "$machine" | sudo tee "$root/etc/hostname" >/dev/null
    offline useradd -m -G wheel -s /bin/bash alice
    # alice's user manager runs from boot, as a signed-in person's does.
    offline install -d -m 0755 /var/lib/systemd/linger
    offline touch /var/lib/systemd/linger/alice
    echo '%wheel ALL=(ALL:ALL) NOPASSWD: ALL' | offline tee /etc/sudoers.d/e2e >/dev/null
    offline chmod 0440 /etc/sudoers.d/e2e
    # No /dev/net/tun in the container: Tailscale's userspace networking instead.
    offline sed -i 's/^FLAGS=.*/FLAGS="--tun=userspace-networking"/' /etc/default/tailscaled
    # An Omarchy computer has ufw on.
    offline sed -i 's/^ENABLED=.*/ENABLED=yes/' /etc/ufw/ufw.conf
    offline systemctl enable ufw.service >/dev/null 2>&1
    # Stand-ins for Omarchy's shell commands (the real ones need the running shell).
    for stub in "$here"/omarchy-stubs/*; do
      sudo install -m 0755 "$stub" "$root/usr/local/bin/$(basename "$stub")"
    done
    # alice's Hyprland configuration, as Omarchy writes one.
    offline runuser -u alice -- install -d -m 0755 /home/alice/.config/hypr
    printf -- '-- Omarchy\nrequire("default.hypr.toggles")\n' | offline runuser -u alice -- tee /home/alice/.config/hypr/hyprland.lua >/dev/null
    echo "Created $root"
    ;;
  boot)
    sudo systemd-run --unit="$machine" --quiet \
      systemd-nspawn -q --boot --private-network --machine="$machine" -D "$root" --capability=CAP_NET_ADMIN
    for _ in $(seq 60); do
      if sudo systemd-run -M "$machine" --quiet --wait --pipe --collect systemctl is-system-running --wait >/dev/null 2>&1; then break; fi
      sleep 1
    done
    inside systemctl is-system-running || true
    ;;
  channel)
    dir=${2:?release directory}
    url=$(sed -n "s/^IBARA_BASE_URL='\(.*\)'$/\1/p" "$dir/install")
    inside systemctl stop channel.service >/dev/null 2>&1 || true
    if [[ $url == http://127.0.0.1:8080 ]]; then
      sudo rm -rf "$root/srv/channel"
      sudo install -d -m 0755 "$root/srv/channel"
      sudo cp "$dir"/* "$root/srv/channel/"
      sudo sh -c 'chmod 0644 "$1"/*' sh "$root/srv/channel"
      sudo systemd-run -M "$machine" --quiet --unit=channel -p DynamicUser=yes \
        python -m http.server 8080 --bind 127.0.0.1 --directory /srv/channel
    elif [[ $url =~ ^https://github\.com/([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)/releases/latest/download$ ]]; then
      repo=${BASH_REMATCH[1]}
      tag=v$(jq -r .version "$dir/stable.json")
      if [[ ! -f $root/srv/tls/ca.pem ]]; then
        # A CA made for this container and trusted by it alone; the host never sees it.
        inside /bin/bash -euc '
          install -d -m 0700 /srv/tls && cd /srv/tls
          openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout ca.key -out ca.pem -days 7 \
            -subj "/CN=ibara E2E test CA" -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign,cRLSign 2>/dev/null
          openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout server.key -out server.csr -subj "/CN=github.com" 2>/dev/null
          printf "subjectAltName=DNS:github.com,DNS:release-assets.githubusercontent.com\n" >san
          openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 7 -extfile san -out server.pem 2>/dev/null
          trust anchor --store ca.pem && update-ca-trust
          printf "127.0.0.1 github.com release-assets.githubusercontent.com\n" >>/etc/hosts'
      fi
      sudo install -D -m 0755 "$here/release-server" "$root/usr/local/bin/release-server"
      sudo rm -rf "$root/srv/releases/$tag"
      sudo install -d -m 0755 "$root/srv/releases/$tag"
      sudo cp "$dir"/* "$root/srv/releases/$tag/"
      sudo sh -c 'chmod 0644 "$1"/*' sh "$root/srv/releases/$tag"
      echo "$tag" | sudo tee "$root/srv/releases/latest" >/dev/null
      sudo systemd-run -M "$machine" --quiet --unit=channel \
        python /usr/local/bin/release-server "$repo" /srv/releases /srv/tls/server.pem /srv/tls/server.key
    else
      echo "$dir/install names $url: neither http://127.0.0.1:8080 nor a GitHub release address." >&2
      exit 1
    fi
    sleep 1
    ;;
  alice)
    # In alice's own user manager (its bus, XDG_RUNTIME_DIR), keeping the exit status.
    sudo systemd-run -M "alice@$machine" --user --quiet --wait --pipe --collect /bin/bash -lc "${2:?command}"
    ;;
  root)
    inside /bin/bash -c "${2:?command}"
    ;;
  stop)
    sudo machinectl poweroff "$machine" 2>/dev/null || true
    for _ in $(seq 30); do sudo machinectl status "$machine" >/dev/null 2>&1 || break; sleep 1; done
    sudo systemctl reset-failed "$machine.service" 2>/dev/null || true
    ;;
  snapshot)
    sudo rm -rf --one-file-system "$root.fresh"
    sudo cp -a --reflink=auto "$root" "$root.fresh"
    ;;
  reset)
    "$0" stop
    sudo rm -rf --one-file-system "$root"
    sudo cp -a --reflink=auto "$root.fresh" "$root"
    ;;
  destroy)
    "$0" stop
    sudo rm -rf --one-file-system "$root" "$root.fresh"
    echo "Removed $root"
    ;;
  *)
    sed -n '2,25p' "$0" >&2
    exit 64
    ;;
esac
