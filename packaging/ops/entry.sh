#!/usr/bin/env bash
# sshd forced command: entry.sh <principal> [controller socket] [gateway key]
# [operator key] [operator key fingerprint]. The optional paths come only from
# root-owned authorized_keys lines and sshd Match blocks. `ibara agent-entry`
# reads SSH_ORIGINAL_COMMAND (mcp, transfer-v1 or operator-v1) and serves it on
# stdio; server policy still enforces the principal allow-list.
set -euo pipefail
principal=${1:-}
[[ "$principal" =~ ^[a-z][a-z0-9_-]{0,63}$ ]] || exit 64
release=$(dirname "$(dirname "$(realpath "$0")")")
exec "$release/bin/ibara" agent-entry "$@"
