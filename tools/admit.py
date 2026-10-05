#!/usr/bin/env python3
from __future__ import annotations

import argparse
import contextlib
import os
import socket
import stat
import struct
import threading
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

XATTR = b"user.io_tier.origin_v1"
ORIGIN = struct.Struct("=QQQqqqq")


def fingerprint(st: os.stat_result) -> bytes:
    return ORIGIN.pack(
        st.st_dev,
        st.st_ino,
        st.st_size,
        st.st_mtime_ns // 1_000_000_000,
        st.st_mtime_ns % 1_000_000_000,
        st.st_ctime_ns // 1_000_000_000,
        st.st_ctime_ns % 1_000_000_000,
    )


class AdmissionWorker:
    def __init__(
        self,
        root: Path,
        max_size: int,
        workers: int,
        parent_budget: int,
        parent_files: int,
        high_watermark: int,
        low_watermark: int,
    ) -> None:
        self.root = root
        self.max_size = max_size
        self.parent_budget = parent_budget
        self.parent_files = parent_files
        self.high_watermark = high_watermark
        self.low_watermark = low_watermark
        self.pool = ThreadPoolExecutor(max_workers=workers, thread_name_prefix="admit")

        st_dev = self.root.stat().st_dev
        devlink = Path(f"/sys/dev/block/{os.major(st_dev)}:{os.minor(st_dev)}")
        try:
            self.mm_stat = devlink.resolve() / "mm_stat"
            if not self.mm_stat.is_file():
                self.mm_stat = None
        except OSError:
            self.mm_stat = None

    def destination(self, src: Path) -> Path:
        if not src.is_absolute():
            raise ValueError("source path must be absolute")
        return self.root / src.as_posix().lstrip("/")

    def valid(self, src_st: os.stat_result, dst: Path) -> bool:
        try:
            dst_st = dst.stat()
            if dst_st.st_size != src_st.st_size:
                return False
            return os.getxattr(dst, XATTR) == fingerprint(src_st)
        except OSError:
            return False

    def memory_used(self) -> int | None:
        if self.mm_stat is None:
            return None
        try:
            fields = self.mm_stat.read_text().split()
            return int(fields[2])
        except (OSError, ValueError, IndexError):
            return None

    def evict_if_needed(self, protect: Path | None = None) -> dict[str, int]:
        before = self.memory_used()
        stats = {
            "mem_before": before or 0,
            "mem_after": before or 0,
            "evicted_files": 0,
            "evicted_dirs": 0,
        }
        if before is None or before <= self.high_watermark:
            return stats

        protected = self.destination(protect) if protect is not None else None
        candidates: list[tuple[int, Path, list[Path]]] = []

        for dirpath, _dirnames, filenames in os.walk(self.root):
            d = Path(dirpath)
            files: list[Path] = []
            newest_atime = 0
            for name in filenames:
                if ".io-tier.tmp." in name:
                    continue
                p = d / name
                try:
                    st = p.stat()
                except OSError:
                    continue
                if not stat.S_ISREG(st.st_mode):
                    continue
                files.append(p)
                newest_atime = max(newest_atime, st.st_atime_ns)
            if files:
                candidates.append((newest_atime, d, files))

        candidates.sort(key=lambda item: item[0])

        for _atime, d, files in candidates:
            if protected is not None and d == protected:
                continue
            for p in files:
                try:
                    p.unlink()
                    stats["evicted_files"] += 1
                except OSError:
                    pass
            try:
                d.rmdir()
                stats["evicted_dirs"] += 1
            except OSError:
                pass

            used = self.memory_used()
            if used is not None:
                stats["mem_after"] = used
                if used <= self.low_watermark:
                    break

        after = self.memory_used()
        if after is not None:
            stats["mem_after"] = after
        return stats

    def copy_one(self, src: Path) -> tuple[str, int]:
        try:
            fd = os.open(src, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
        except OSError:
            return ("open_error", 0)

        tmp: Path | None = None
        try:
            before = os.fstat(fd)
            if not stat.S_ISREG(before.st_mode) or before.st_size > self.max_size:
                return ("ineligible", 0)

            dst = self.destination(src)
            if self.valid(before, dst):
                return ("valid", before.st_size)

            dst.parent.mkdir(parents=True, exist_ok=True)
            tmp = dst.parent / (f".{dst.name}.io-tier.tmp.{os.getpid()}.{threading.get_ident()}")

            out = os.open(
                tmp,
                os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_CLOEXEC,
                stat.S_IMODE(before.st_mode),
            )
            try:
                offset = 0
                while offset < before.st_size:
                    chunk = os.pread(fd, min(1 << 20, before.st_size - offset), offset)
                    if not chunk:
                        break
                    view = memoryview(chunk)
                    written = 0
                    while written < len(view):
                        written += os.write(out, view[written:])
                    offset += len(chunk)

                if offset != before.st_size:
                    return ("short_read", 0)

                after = os.fstat(fd)
                if fingerprint(after) != fingerprint(before):
                    return ("changed", 0)

                with contextlib.suppress(PermissionError):
                    os.fchown(out, before.st_uid, before.st_gid)
                os.fchmod(out, stat.S_IMODE(before.st_mode))
                os.setxattr(out, XATTR, fingerprint(after))
            finally:
                os.close(out)

            os.utime(
                tmp,
                ns=(before.st_atime_ns, before.st_mtime_ns),
                follow_symlinks=False,
            )
            os.replace(tmp, dst)
            tmp = None

            try:
                out = os.open(dst, os.O_RDONLY | os.O_CLOEXEC)
                try:
                    if hasattr(os, "posix_fadvise"):
                        os.posix_fadvise(out, 0, 0, os.POSIX_FADV_DONTNEED)
                finally:
                    os.close(out)
            except OSError:
                pass

            return ("copied", before.st_size)
        except OSError:
            return ("copy_error", 0)
        finally:
            os.close(fd)
            if tmp is not None:
                with contextlib.suppress(OSError):
                    tmp.unlink()

    def admit_dir(self, directory: Path) -> dict[str, int]:
        stats: dict[str, int] = {}
        candidates: list[tuple[int, Path]] = []
        try:
            with os.scandir(directory) as it:
                for ent in it:
                    try:
                        if not ent.is_file(follow_symlinks=False):
                            continue
                        st = ent.stat(follow_symlinks=False)
                    except OSError:
                        continue
                    if st.st_size <= self.max_size:
                        candidates.append((st.st_size, Path(ent.path)))
        except OSError:
            return {"scan_error": 1}

        # Prefer the cheap seek-heavy microfiles and cap every activation.
        # This prevents a single huge flat directory from consuming the tier.
        candidates.sort(key=lambda item: (item[0], str(item[1])))
        selected: list[Path] = []
        selected_bytes = 0
        for size, path in candidates:
            if len(selected) >= self.parent_files:
                break
            if selected_bytes + size > self.parent_budget:
                break
            selected.append(path)
            selected_bytes += size

        jobs = [self.pool.submit(self.copy_one, path) for path in selected]
        copied_bytes = 0
        for job in as_completed(jobs):
            status, size = job.result()
            stats[status] = stats.get(status, 0) + 1
            if status == "copied":
                copied_bytes += size
        stats["candidate_files"] = len(candidates)
        stats["selected_files"] = len(selected)
        stats["selected_bytes"] = selected_bytes
        stats["copied_bytes"] = copied_bytes
        return stats


def serve(args: argparse.Namespace) -> None:
    worker = AdmissionWorker(
        args.micro_root,
        args.max_size,
        args.workers,
        args.parent_budget,
        args.parent_files,
        args.high_watermark,
        args.low_watermark,
    )
    startup_eviction = worker.evict_if_needed()
    if startup_eviction["evicted_files"]:
        print(f"startup eviction {startup_eviction}", flush=True)

    sock_path = args.socket
    sock_path.parent.mkdir(parents=True, exist_ok=True)
    with contextlib.suppress(FileNotFoundError):
        sock_path.unlink()

    sock = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
    sock.bind(str(sock_path))
    os.chmod(sock_path, 0o666)

    print(
        f"admission worker ready socket={sock_path} micro_root={args.micro_root} "
        f"max_size={args.max_size} workers={args.workers}",
        flush=True,
    )

    last: dict[str, float] = {}
    while True:
        data = sock.recv(4096)
        try:
            directory = Path(data.decode())
        except UnicodeDecodeError:
            continue
        if not directory.is_absolute():
            continue

        now = time.monotonic()
        key = str(directory)
        if now - last.get(key, -1e9) < args.cooldown:
            continue
        last[key] = now

        t0 = time.monotonic()
        result = worker.admit_dir(directory)
        eviction = worker.evict_if_needed(directory)
        result.update({f"evict_{k}": v for k, v in eviction.items()})
        print(
            f"admit dir={directory} elapsed_ms={(time.monotonic() - t0) * 1000:.1f} {result}",
            flush=True,
        )


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--micro-root", type=Path, default=Path("/mnt/io-tier-zram-full"))
    ap.add_argument("--max-size", type=int, default=2 * 1024 * 1024)
    ap.add_argument("--workers", type=int, default=4)
    ap.add_argument("--parent-budget", type=int, default=32 * 1024 * 1024)
    ap.add_argument("--parent-files", type=int, default=4096)
    ap.add_argument("--high-watermark", type=int, default=180 * 1024 * 1024)
    ap.add_argument("--low-watermark", type=int, default=160 * 1024 * 1024)
    sub = ap.add_subparsers(dest="command", required=True)

    one = sub.add_parser("admit")
    one.add_argument("directory", type=Path)

    daemon = sub.add_parser("serve")
    daemon.add_argument("--socket", type=Path, default=Path("/run/io-tierfs-admit.sock"))
    daemon.add_argument("--cooldown", type=float, default=2.0)

    args = ap.parse_args()
    if args.command == "admit":
        worker = AdmissionWorker(
            args.micro_root,
            args.max_size,
            args.workers,
            args.parent_budget,
            args.parent_files,
            args.high_watermark,
            args.low_watermark,
        )
        t0 = time.monotonic()
        result = worker.admit_dir(args.directory)
        print(f"elapsed_ms={(time.monotonic() - t0) * 1000:.1f} {result}")
    else:
        serve(args)


if __name__ == "__main__":
    main()
