#!/usr/bin/env python3
"""Reject unusable Arch release archives before signing or publication."""
import os
from pathlib import Path
import subprocess
import sys
import tarfile

if len(sys.argv) != 3:
    sys.exit("usage: check-packages.py VERSION DIRECTORY")
version, directory = sys.argv[1], Path(sys.argv[2])
executables = {
    "ibara": ("usr/lib/ibara/bin/ibara", "usr/lib/ibara/bin/ibarad", "usr/lib/ibara/bin/ibara-screen"),
    "ibara-stream": ("usr/bin/ibara-stream",),
    "ibara-view": ("usr/bin/ibara-view",),
}
for name, required in executables.items():
    package = directory / f"{name}-{version}-x86_64.pkg.tar.zst"
    try:
        subprocess.run(["zstd", "-q", "-t", str(package)], check=True)
        query = subprocess.check_output(["pacman", "-Qp", str(package)], text=True).strip()
        if query != f"{name} {version}":
            raise ValueError(f"unexpected pacman identity: {query}")
        proc = subprocess.Popen(["zstd", "-q", "-d", "-c", str(package)], stdout=subprocess.PIPE)
        seen = set()
        with tarfile.open(fileobj=proc.stdout, mode="r|") as archive:
            for item in archive:
                seen.add(item.name)
                if item.name.startswith("usr/") and (item.uid != 0 or item.gid != 0):
                    raise ValueError(f"{item.name} is owned by {item.uid}:{item.gid}, not root")
                if item.name.startswith("usr/") and item.isdir() and item.mode & 0o755 != 0o755:
                    raise ValueError(f"{item.name} is not traversable by all users: {item.mode:o}")
                if item.name.startswith("usr/") and item.isfile() and item.mode & 0o444 != 0o444:
                    raise ValueError(f"{item.name} is not readable by all users: {item.mode:o}")
                if item.name in required and (not item.isfile() or item.mode & 0o755 != 0o755):
                    raise ValueError(f"{item.name} is not executable by all users: {item.mode:o}")
        proc.stdout.read()
        proc.stdout.close()
        if proc.wait() != 0:
            raise ValueError("decompression failed")
        missing = set(required) | {".PKGINFO", ".MTREE"}
        missing -= seen
        if missing:
            raise ValueError(f"missing expected archive entries: {sorted(missing)}")
    except (OSError, subprocess.CalledProcessError, tarfile.TarError, ValueError) as error:
        sys.exit(f"{package}: package validation failed: {error}")
    print(f"{package.name}: compression, pacman identity, archive ownership and permissions OK")
