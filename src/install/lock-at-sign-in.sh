#!/bin/bash
# Written by `ibara unattended-boot lock-at-sign-in on`. Omarchy signs this
# account in without a password once the disk is unlocked, so this locks the
# screen right after each sign-in (Omarchy runs the hooks in
# ~/.config/omarchy/hooks/post-boot.d/ then). The account password unlocks it.
# To stop: ibara unattended-boot lock-at-sign-in off, or delete this file.

# The Omarchy shell may still be starting: ask until it has taken the lock.
for _ in {1..60}; do
  [[ $(omarchy-shell lock lock 2>/dev/null) == ok ]] && exit 0
  sleep 0.5
done
echo "ibara could not lock the screen at sign-in: the Omarchy shell did not take the lock." >&2
exit 1
