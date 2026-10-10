#!/usr/bin/env python3
"""Stage the shared Console with the standalone desktop host, no Omarchy shell."""
from pathlib import Path
import shutil
import sys

def main():
    source = Path(__file__).resolve().parent.parent
    dest = Path(sys.argv[1]).resolve()
    dest.mkdir(parents=True, exist_ok=True)
    shared = dest / 'plugin'
    shared.mkdir(exist_ok=True)
    for path in source.iterdir():
        if path.is_file() and path.suffix in ('.qml', '.js', '.json', '.svg'):
            shutil.copy2(path, shared / path.name)
    for name in ('Commons', 'Ui'):
        shutil.copytree(source / 'standalone' / name, dest / name, dirs_exist_ok=True)
    for path in (source / 'standalone').glob('*.qml'):
        shutil.copy2(path, dest / path.name)
    # Source permissions belong to the checkout; installed assets must be readable.
    dest.chmod(0o755)
    for path in dest.rglob('*'):
        path.chmod(0o755 if path.is_dir() else 0o644)

if __name__ == '__main__':
    main()
