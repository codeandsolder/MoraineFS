#!/usr/bin/env python3
from __future__ import annotations

import contextlib
import os
import shutil
from pathlib import Path

ROOT = Path("/var/cache/io-tierfs/range-cache")


def main() -> int:
    ROOT.mkdir(parents=True, exist_ok=True)
    removed_dirs = 0
    removed_bytes = 0
    kept = 0

    for entry in ROOT.iterdir():
        if not entry.is_dir() or not entry.name.startswith("pid-"):
            continue
        try:
            pid = int(entry.name[4:])
        except ValueError:
            continue

        if Path(f"/proc/{pid}").exists():
            kept += 1
            continue

        size = 0
        for dirpath, _dirs, files in os.walk(entry):
            for name in files:
                p = Path(dirpath) / name
                with contextlib.suppress(OSError):
                    size += p.stat().st_size
        shutil.rmtree(entry, ignore_errors=False)
        removed_dirs += 1
        removed_bytes += size

    print(f"removed_dirs={removed_dirs} removed_MiB={removed_bytes / 2**20:.1f} active_dirs={kept}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
