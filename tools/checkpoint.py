#!/usr/bin/env python3
from __future__ import annotations

import argparse
import contextlib
import ctypes
import errno
import heapq
import os
import queue
import signal
import socket
import stat
import threading
import time
from dataclasses import dataclass
from pathlib import Path

CHUNK = 1024 * 1024
Generation = tuple[int, int, int]

_LIBC = ctypes.CDLL(None, use_errno=True)
_SYNCFS = _LIBC.syncfs
_SYNCFS.argtypes = [ctypes.c_int]
_SYNCFS.restype = ctypes.c_int


def generation(st: os.stat_result) -> Generation:
    return (st.st_size, st.st_mtime_ns, st.st_ctime_ns)


def fsync_dir(path: Path) -> None:
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def syncfs_fd(fd: int) -> None:
    if _SYNCFS(fd) != 0:
        err = ctypes.get_errno()
        raise OSError(err, os.strerror(err))


@dataclass
class Prepared:
    source: str
    start_seq: int
    clean_gen: Generation
    copied: int
    copy_s: float
    changed_during_copy: bool
    canonical_fd: int = -1
    sync_dirs: tuple[Path, ...] = ()


class Checkpointer:
    def __init__(
        self,
        root: Path,
        state_root: Path,
        namespace_root: Path,
        rename_root: Path,
        source_prefix: str,
        batch_max_files: int,
        batch_delay: float,
        durability: str = "syncfs",
        settle_delay: float = 0.0,
    ) -> None:
        self.root = root
        self.state_root = state_root
        self.namespace_root = namespace_root
        self.rename_root = rename_root
        self.source_prefix = source_prefix
        self.batch_max_files = batch_max_files
        self.batch_delay = batch_delay
        if settle_delay < 0:
            raise ValueError("settle_delay must be >= 0")
        self.settle_delay = settle_delay
        if durability not in {"syncfs", "file"}:
            raise ValueError(f"unsupported durability mode: {durability}")
        self.durability = durability
        self.q: queue.Queue[str | None] = queue.Queue()
        self.lock = threading.Lock()
        self.seq: dict[str, int] = {}
        self.pending: set[str] = set()
        self.last_changed: dict[str, float] = {}
        self.deferred: list[tuple[float, int, str]] = []
        self.deferred_serial = 0
        self.stopping = threading.Event()
        sync_root = source_prefix.rstrip("/") or "/"
        self.sync_fd = os.open(
            sync_root,
            os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC,
        )

    def close(self) -> None:
        os.close(self.sync_fd)

    def overlay_path(self, source: str) -> Path:
        return self.root / source.lstrip("/")

    def state_path(self, source: str) -> Path:
        rel = Path(source.lstrip("/"))
        return self.state_root / rel.parent / (rel.name + ".state")

    def marker_path(self, source: str) -> Path:
        rel = Path(source.lstrip("/"))
        return self.namespace_root / rel.parent / (rel.name + ".created")

    def marker_exists(self, source: str) -> bool:
        try:
            return self.marker_path(source).is_file()
        except OSError:
            return False

    def rename_marker_path(self, source: str) -> Path:
        rel = Path(source.lstrip("/"))
        return self.rename_root / rel.parent / (rel.name + ".rename")

    def rename_marker_exists(self, source: str) -> bool:
        try:
            return self.rename_marker_path(source).is_file()
        except OSError:
            return False

    def rename_ready_path(self, source: str) -> Path:
        rel = Path(source.lstrip("/"))
        return self.rename_root / rel.parent / (rel.name + ".rename.ready")

    def rename_dest_backup_path(self, source: str) -> Path:
        rel = Path(source.lstrip("/"))
        return self.rename_root / rel.parent / (rel.name + ".rename.dst-overlay")

    def rename_ready_exists(self, source: str) -> bool:
        try:
            return self.rename_ready_path(source).is_file()
        except OSError:
            return False

    def mark_rename_ready(self, source: str) -> None:
        p = self.rename_ready_path(source)
        p.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(
            p,
            os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_CLOEXEC | os.O_NOFOLLOW,
            0o600,
        )
        try:
            os.write(fd, b"ready-v1\n")
            os.fsync(fd)
        finally:
            os.close(fd)
        fsync_dir(p.parent)

    def clear_rename_ready(self, source: str) -> None:
        p = self.rename_ready_path(source)
        try:
            p.unlink()
        except FileNotFoundError:
            return
        fsync_dir(p.parent)

    def read_rename_marker(self, source: str) -> str | None:
        try:
            raw = self.rename_marker_path(source).read_bytes()
        except OSError:
            return None
        # C writes the pathname bytes exactly, without a delimiter. os.fsdecode
        # preserves arbitrary Linux pathname bytes via surrogateescape.
        if not raw or b"\0" in raw:
            return None
        old = os.fsdecode(raw)
        if (
            not old.startswith("/")
            or not old.startswith(self.source_prefix)
            or old == source
            or os.path.normpath(old) != old
        ):
            return None
        return old

    def clear_rename_marker(self, source: str) -> None:
        p = self.rename_marker_path(source)
        try:
            p.unlink()
        except FileNotFoundError:
            return
        fsync_dir(p.parent)

    def read_state(self, source: str) -> Generation | None:
        p = self.state_path(source)
        try:
            fields = p.read_text().split()
            if len(fields) != 3:
                return None
            return (int(fields[0]), int(fields[1]), int(fields[2]))
        except (OSError, ValueError):
            return None

    def write_state(self, source: str, gen: Generation) -> None:
        p = self.state_path(source)
        p.parent.mkdir(parents=True, exist_ok=True)
        tmp = p.with_name(f".{p.name}.tmp.{os.getpid()}.{threading.get_ident()}")
        data = f"{gen[0]} {gen[1]} {gen[2]}\n".encode()
        fd = os.open(
            tmp,
            os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_CLOEXEC,
            0o600,
        )
        try:
            view = memoryview(data)
            while view:
                n = os.write(fd, view)
                view = view[n:]
            os.fsync(fd)
        finally:
            os.close(fd)
        os.replace(tmp, p)
        fsync_dir(p.parent)

    def clear_marker(self, source: str) -> None:
        p = self.marker_path(source)
        try:
            p.unlink()
        except FileNotFoundError:
            return
        fsync_dir(p.parent)

    def discard_orphan(self, source: str) -> None:
        for p in (self.overlay_path(source), self.state_path(source)):
            with contextlib.suppress(FileNotFoundError):
                p.unlink()

    @staticmethod
    def _unlink_if_exists(path: Path) -> None:
        with contextlib.suppress(FileNotFoundError):
            path.unlink()

    def finalize_rename(self, source: str) -> None:
        old = self.read_rename_marker(source)
        if old is None:
            if self.rename_marker_exists(source):
                raise OSError(errno.EINVAL, "invalid rename marker")
            return

        # The overlay pathname must itself be durable before the intent is
        # forgotten.  The preceding syncfs made the HDD rename + data durable.
        overlay = self.overlay_path(source)
        if overlay.exists():
            fsync_dir(overlay.parent)

        # A create marker belongs to the old pathname only until the rename is
        # durable.  Leaving it around after marker removal could resurrect old.
        if self.marker_exists(old):
            self.clear_marker(old)
        if self.rename_ready_exists(source):
            self.clear_rename_ready(source)
        self._unlink_if_exists(self.rename_dest_backup_path(source))
        fsync_dir(self.rename_dest_backup_path(source).parent)
        self.clear_rename_marker(source)

    def recover_pending_renames(
        self,
        *,
        include_incomplete: bool = False,
    ) -> dict[str, int]:
        counts: dict[str, int] = {}
        if not self.rename_root.exists():
            return counts

        markers = sorted(self.rename_root.rglob("*.rename"))
        for marker in markers:
            try:
                rel = marker.relative_to(self.rename_root)
            except ValueError:
                continue
            new_rel = rel.parent / rel.name.removesuffix(".rename")
            new = "/" + new_rel.as_posix()
            old = self.read_rename_marker(new)
            if old is None:
                counts["invalid_marker"] = counts.get("invalid_marker", 0) + 1
                continue

            ready = self.rename_ready_exists(new)
            if not ready and not include_incomplete:
                # A live FUSE process may still be between the two namespace
                # moves. Only an explicitly offline/pre-mount recovery pass may
                # roll an intent without the ready phase forward.
                counts["deferred_incomplete"] = counts.get("deferred_incomplete", 0) + 1
                continue

            old_overlay = self.overlay_path(old)
            new_overlay = self.overlay_path(new)
            try:
                old_exists = old_overlay.is_file()
                new_exists = new_overlay.is_file()

                if old_exists:
                    new_overlay.parent.mkdir(parents=True, exist_ok=True)
                    os.replace(old_overlay, new_overlay)
                    fsync_dir(new_overlay.parent)
                    if old_overlay.parent != new_overlay.parent:
                        fsync_dir(old_overlay.parent)
                    new_exists = True
                elif not new_exists:
                    # A completed rename may be unlinked before asynchronous
                    # checkpointing ever sees it. If both namespace endpoints
                    # and both overlays are gone, there is no transaction left
                    # to recover; retaining intent+ready would poison future
                    # reuse of the destination pathname with EBUSY forever.
                    if not os.path.lexists(old) and not os.path.lexists(new):
                        if self.marker_exists(old):
                            self.clear_marker(old)
                        if self.marker_exists(new):
                            self.clear_marker(new)
                        if self.rename_ready_exists(new):
                            self.clear_rename_ready(new)
                        self._unlink_if_exists(self.rename_dest_backup_path(new))
                        fsync_dir(self.rename_dest_backup_path(new).parent)
                        self.clear_rename_marker(new)
                        counts["deleted_transaction"] = counts.get("deleted_transaction", 0) + 1
                        continue
                    counts["missing_overlay"] = counts.get("missing_overlay", 0) + 1
                    continue

                # Any pre-rename generation is invalid for the destination.
                # The durable intent remains until a new state is written, so
                # these unlinks do not themselves need to carry correctness.
                for state_path in (self.state_path(old), self.state_path(new)):
                    self._unlink_if_exists(state_path)

                old_canonical_exists = os.path.lexists(old)
                new_canonical_exists = os.path.lexists(new)
                if old_canonical_exists:
                    os.replace(old, new)
                elif not new_canonical_exists:
                    st = new_overlay.stat()
                    fd = os.open(
                        new,
                        os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC | os.O_NOFOLLOW,
                        stat.S_IMODE(st.st_mode),
                    )
                    try:
                        os.fchmod(fd, stat.S_IMODE(st.st_mode))
                        with contextlib.suppress(OSError):
                            os.fchown(fd, st.st_uid, st.st_gid)
                    finally:
                        os.close(fd)

                # A dirty destination overlay may have been moved aside before
                # the source overlay was installed. Recovery always rolls the
                # rename forward, so the replaced destination inode is no
                # longer part of the namespace and this private backup can go.
                backup = self.rename_dest_backup_path(new)
                if backup.exists():
                    backup.unlink()
                    fsync_dir(backup.parent)

                # Do not forget the intent here. Publishing ready merely says
                # both namespace moves have completed. scan_existing() will
                # force a normal data checkpoint; only its post-syncfs
                # finalize_rename() may clear ready + intent.
                if not ready:
                    self.mark_rename_ready(new)
                    counts["recovered_incomplete"] = counts.get("recovered_incomplete", 0) + 1
                else:
                    counts["ready_pending"] = counts.get("ready_pending", 0) + 1
            except OSError as exc:
                key = f"error_{exc.errno or 0}"
                counts[key] = counts.get(key, 0) + 1

        # Ready/backup auxiliaries cannot drive recovery without an intent.
        # Prune them defensively so an interrupted foreground cleanup does not
        # accumulate private metadata forever.
        for ready_path in sorted(self.rename_root.rglob("*.rename.ready")):
            marker_path = ready_path.with_name(ready_path.name.removesuffix(".ready"))
            if marker_path.exists():
                continue
            try:
                ready_path.unlink()
                fsync_dir(ready_path.parent)
                counts["orphan_ready_pruned"] = counts.get("orphan_ready_pruned", 0) + 1
            except OSError:
                pass

        for backup_path in sorted(self.rename_root.rglob("*.rename.dst-overlay")):
            marker_path = backup_path.with_name(backup_path.name.removesuffix(".dst-overlay"))
            if marker_path.exists():
                continue
            try:
                backup_path.unlink()
                fsync_dir(backup_path.parent)
                counts["orphan_backup_pruned"] = counts.get("orphan_backup_pruned", 0) + 1
            except OSError:
                pass

        return counts

    def enqueue(self, source: str, *, changed_event: bool) -> None:
        if not source.startswith("/") or not source.startswith(self.source_prefix):
            return
        now = time.monotonic()
        with self.lock:
            if changed_event:
                self.seq[source] = self.seq.get(source, 0) + 1
                self.last_changed[source] = now
            else:
                self.seq.setdefault(source, 0)
                self.last_changed.setdefault(source, now)
            if source in self.pending:
                return
            self.pending.add(source)
            self.q.put(source)

    def _settle_due_locked(self, source: str) -> float:
        return self.last_changed.get(source, 0.0) + self.settle_delay

    def _defer_source(self, source: str, due: float) -> None:
        with self.lock:
            self.deferred_serial += 1
            heapq.heappush(
                self.deferred,
                (due, self.deferred_serial, source),
            )

    def _defer_if_unsettled(self, source: str) -> bool:
        if self.settle_delay <= 0:
            return False
        now = time.monotonic()
        with self.lock:
            due = self._settle_due_locked(source)
        if due <= now:
            return False
        self._defer_source(source, due)
        return True

    def _promote_due_deferred(self) -> None:
        if self.settle_delay <= 0:
            return
        while True:
            now = time.monotonic()
            with self.lock:
                if not self.deferred or self.deferred[0][0] > now:
                    return
                _queued_due, _serial, source = heapq.heappop(self.deferred)
                actual_due = self._settle_due_locked(source)
                if actual_due > now:
                    self.deferred_serial += 1
                    heapq.heappush(
                        self.deferred,
                        (actual_due, self.deferred_serial, source),
                    )
                    continue
            self.q.put(source)

    def _next_deferred_wait(self, maximum: float) -> float:
        if self.settle_delay <= 0:
            return maximum
        with self.lock:
            if not self.deferred:
                return maximum
            due = self.deferred[0][0]
        return max(0.0, min(maximum, due - time.monotonic()))

    def open_or_recover_canonical(
        self,
        source: str,
        overlay_st: os.stat_result,
    ) -> int:
        try:
            return os.open(
                source,
                os.O_WRONLY | os.O_CLOEXEC | os.O_NOFOLLOW,
            )
        except OSError as exc:
            recoverable_namespace = self.marker_exists(source) or (
                self.rename_marker_exists(source) and self.rename_ready_exists(source)
            )
            if exc.errno != errno.ENOENT or not recoverable_namespace:
                raise

        # A durable create intent or completed rename intent means the
        # application was told that this logical path exists even when its
        # HDD final component is intentionally deferred. Recreate only the
        # missing final component; the selected durability barrier later
        # persists both this directory entry and file data.
        fd = os.open(
            source,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC | os.O_NOFOLLOW,
            stat.S_IMODE(overlay_st.st_mode),
        )
        with contextlib.suppress(OSError):
            os.fchmod(fd, stat.S_IMODE(overlay_st.st_mode))
        with contextlib.suppress(OSError):
            os.fchown(fd, overlay_st.st_uid, overlay_st.st_gid)
        return fd

    def prepare_copy(
        self,
        source: str,
        start_seq: int,
    ) -> tuple[str, Prepared | None]:
        # An intent without the ready phase means FUSE may still be between
        # the overlay move and canonical rename. Never checkpoint that window.
        if self.rename_marker_exists(source) and not self.rename_ready_exists(source):
            return ("rename_in_progress", None)

        overlay = self.overlay_path(source)
        try:
            ofd = os.open(
                overlay,
                os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW,
            )
        except OSError:
            return ("overlay_missing", None)

        cfd = -1
        started = time.monotonic()
        copied = 0
        try:
            before = os.fstat(ofd)
            if not stat.S_ISREG(before.st_mode):
                return ("overlay_not_regular", None)
            before_gen = generation(before)

            try:
                cfd = self.open_or_recover_canonical(source, before)
            except OSError as exc:
                if exc.errno == errno.ENOENT and not self.marker_exists(source):
                    self.discard_orphan(source)
                    return ("discarded_deleted", None)
                return (f"canonical_open_error:{exc.errno}", None)

            offset = 0
            while offset < before.st_size:
                data = os.pread(
                    ofd,
                    min(CHUNK, before.st_size - offset),
                    offset,
                )
                if not data:
                    return ("short_read", None)
                view = memoryview(data)
                written = 0
                while written < len(view):
                    n = os.pwrite(cfd, view[written:], offset + written)
                    if n <= 0:
                        return ("short_write", None)
                    written += n
                offset += len(data)
                copied += len(data)

            os.ftruncate(cfd, before.st_size)
            with contextlib.suppress(OSError):
                os.fchmod(cfd, stat.S_IMODE(before.st_mode))
            with contextlib.suppress(OSError):
                os.fchown(cfd, before.st_uid, before.st_gid)
            with contextlib.suppress(OSError):
                os.utime(cfd, ns=(before.st_atime_ns, before.st_mtime_ns))

            sync_dirs: set[Path] = set()
            if self.durability == "file":
                # A create marker means the canonical directory entry itself
                # is not yet known durable, even when open() found it.
                if self.marker_exists(source):
                    sync_dirs.add(Path(source).parent)

                # A ready rename has completed both namespace moves in FUSE,
                # but the old/new parent directories still need an explicit
                # durability barrier before the intent can be forgotten.
                if self.rename_marker_exists(source):
                    if not self.rename_ready_exists(source):
                        return ("rename_in_progress", None)
                    old = self.read_rename_marker(source)
                    if old is None:
                        return ("invalid_rename_marker", None)
                    sync_dirs.add(Path(old).parent)
                    sync_dirs.add(Path(source).parent)

            after = os.fstat(ofd)
            changed = generation(after) != before_gen
            canonical_fd = -1
            if self.durability == "file":
                canonical_fd = cfd
                cfd = -1
            return (
                "prepared",
                Prepared(
                    source=source,
                    start_seq=start_seq,
                    clean_gen=before_gen,
                    copied=copied,
                    copy_s=time.monotonic() - started,
                    changed_during_copy=changed,
                    canonical_fd=canonical_fd,
                    sync_dirs=tuple(sorted(sync_dirs)),
                ),
            )
        except OSError as exc:
            return (f"io_error:{exc.errno}", None)
        finally:
            if cfd != -1:
                os.close(cfd)
            os.close(ofd)

    def overlay_generation(self, source: str) -> Generation | None:
        try:
            st = self.overlay_path(source).stat()
            if not stat.S_ISREG(st.st_mode):
                return None
            return generation(st)
        except OSError:
            return None

    def finish_source(
        self,
        source: str,
        *,
        retry: bool,
        expected_seq: int | None = None,
    ) -> None:
        # Hold the same lock as enqueue() while deciding whether to drop the
        # pending bit. This closes the event-after-final-check race: an event
        # either increments seq before this check (so we requeue), or arrives
        # after pending is cleared (so enqueue() creates a fresh queue item).
        with self.lock:
            changed = expected_seq is not None and self.seq.get(source, 0) != expected_seq
            if (retry or changed) and not self.stopping.is_set():
                self.q.put(source)
            else:
                self.pending.discard(source)

    def gather_batch(self, first: str) -> list[str]:
        batch = [first]
        deadline = time.monotonic() + self.batch_delay
        while len(batch) < self.batch_max_files:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            try:
                item = self.q.get(timeout=remaining)
            except queue.Empty:
                break
            if item is None:
                # Preserve shutdown sentinel for the next loop.
                self.q.put(None)
                self.q.task_done()
                break
            if self._defer_if_unsettled(item):
                self.q.task_done()
                continue
            batch.append(item)
        return batch

    def batch_worker(self) -> None:
        while not self.stopping.is_set():
            self._promote_due_deferred()
            try:
                first = self.q.get(timeout=self._next_deferred_wait(1.0))
            except queue.Empty:
                continue
            if first is None:
                self.q.task_done()
                return
            if self._defer_if_unsettled(first):
                self.q.task_done()
                continue

            batch = self.gather_batch(first)
            prepared: list[Prepared] = []
            status_counts: dict[str, int] = {}
            copy_started = time.monotonic()

            for source in batch:
                if self._defer_if_unsettled(source):
                    status_counts["settling"] = status_counts.get("settling", 0) + 1
                    continue
                with self.lock:
                    start_seq = self.seq.get(source, 0)
                status, item = self.prepare_copy(source, start_seq)
                status_counts[status] = status_counts.get(status, 0) + 1
                if item is not None:
                    prepared.append(item)
                else:
                    self.finish_source(source, retry=False, expected_seq=start_seq)

            copy_s = time.monotonic() - copy_started
            sync_s = 0.0
            sync_ok = True
            sync_error = ""

            if prepared:
                s0 = time.monotonic()
                try:
                    if self.durability == "syncfs":
                        syncfs_fd(self.sync_fd)
                    else:
                        sync_dirs: set[Path] = set()
                        for item in prepared:
                            if item.canonical_fd < 0:
                                raise OSError(errno.EBADF, "missing canonical fd")
                            os.fsync(item.canonical_fd)
                            sync_dirs.update(item.sync_dirs)
                        for directory in sorted(sync_dirs):
                            fsync_dir(directory)
                except OSError as exc:
                    sync_ok = False
                    sync_error = f"{exc.errno}:{exc.strerror}"
                finally:
                    if self.durability == "file":
                        for item in prepared:
                            if item.canonical_fd >= 0:
                                with contextlib.suppress(OSError):
                                    os.close(item.canonical_fd)
                                item.canonical_fd = -1
                sync_s = time.monotonic() - s0

            clean = 0
            retry_count = 0
            state_errors = 0

            for item in prepared:
                with self.lock:
                    current_seq = self.seq.get(item.source, 0)
                current_gen = self.overlay_generation(item.source)
                retry = (
                    not sync_ok
                    or item.changed_during_copy
                    or current_seq != item.start_seq
                    or current_gen != item.clean_gen
                )

                if not retry:
                    try:
                        self.write_state(item.source, item.clean_gen)
                        # The selected durability barrier has made canonical
                        # file data and any changed HDD namespace durable, so a
                        # create marker may now go.
                        if self.marker_exists(item.source):
                            self.clear_marker(item.source)
                        if self.rename_marker_exists(item.source):
                            self.finalize_rename(item.source)
                        clean += 1
                    except OSError:
                        state_errors += 1
                        retry = True

                if retry:
                    retry_count += 1
                self.finish_source(
                    item.source,
                    retry=retry,
                    expected_seq=item.start_seq,
                )

            for _ in batch:
                self.q.task_done()

            print(
                f"batch files={len(batch)} prepared={len(prepared)} "
                f"clean={clean} retry={retry_count} state_errors={state_errors} "
                f"copy_MiB={sum(x.copied for x in prepared) / 2**20:.3f} "
                f"copy_ms={copy_s * 1000:.1f} durability={self.durability} "
                f"sync_ms={sync_s * 1000:.1f} "
                f"sync_ok={int(sync_ok)} sync_error={sync_error or '-'} "
                f"status={dict(sorted(status_counts.items()))}",
                flush=True,
            )

    def prune_states(self) -> int:
        removed = 0
        if not self.state_root.exists():
            return 0
        for dirpath, _dirnames, filenames in os.walk(self.state_root):
            d = Path(dirpath)
            for name in filenames:
                if not name.endswith(".state"):
                    continue
                p = d / name
                rel = p.relative_to(self.state_root)
                overlay_rel = rel.parent / name.removesuffix(".state")
                if (self.root / overlay_rel).exists():
                    continue
                try:
                    p.unlink()
                    removed += 1
                except OSError:
                    pass
        return removed

    def scan_existing(self) -> tuple[int, int, int]:
        seen = 0
        dirty = 0
        if not self.root.exists():
            return (0, 0, self.prune_states())

        for dirpath, _dirnames, filenames in os.walk(self.root):
            d = Path(dirpath)
            for name in filenames:
                if ".io-tier-tmp." in name:
                    continue
                p = d / name
                try:
                    st = p.stat()
                    if not stat.S_ISREG(st.st_mode):
                        continue
                    rel = p.relative_to(self.root)
                except (OSError, ValueError):
                    continue

                source = "/" + rel.as_posix()
                seen += 1
                clean_state = self.read_state(source) == generation(st)
                created_marker = self.marker_exists(source)
                rename_marker = self.rename_marker_exists(source)
                rename_ready = self.rename_ready_exists(source)
                if rename_marker and not rename_ready:
                    # FUSE is still applying the foreground namespace move.
                    continue
                if clean_state and not created_marker and not rename_marker:
                    continue
                self.enqueue(source, changed_event=False)
                dirty += 1

        return (seen, dirty, self.prune_states())


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument(
        "--root",
        type=Path,
        default=Path("/var/lib/io-tierfs/writeback"),
    )
    ap.add_argument(
        "--state-root",
        type=Path,
        default=Path("/var/lib/io-tierfs/checkpoint-state"),
    )
    ap.add_argument(
        "--namespace-root",
        type=Path,
        default=Path("/var/lib/io-tierfs/namespace-state"),
    )
    ap.add_argument(
        "--rename-root",
        type=Path,
        default=Path("/var/lib/io-tierfs/rename-state"),
    )
    ap.add_argument(
        "--socket",
        type=Path,
        default=Path("/run/io-tierfs-checkpoint.sock"),
    )
    ap.add_argument("--source-prefix", default="/srv/scratch/")
    # Kept for unit-file/backward compatibility; batching intentionally uses
    # one HDD writer so several workers cannot manufacture competing flushes.
    ap.add_argument("--workers", type=int, default=1)
    ap.add_argument("--batch-max-files", type=int, default=64)
    ap.add_argument("--batch-delay", type=float, default=0.25)
    ap.add_argument(
        "--settle-delay",
        type=float,
        default=0.0,
        help=(
            "minimum quiet age in seconds since the last change before an "
            "overlay may be copied; 0 preserves immediate checkpointing"
        ),
    )
    ap.add_argument(
        "--durability",
        choices=("syncfs", "file"),
        default="syncfs",
        help=(
            "HDD durability barrier: whole-filesystem syncfs (legacy) or "
            "fsync copied files plus only changed namespace directories"
        ),
    )
    ap.add_argument("--scan-interval", type=float, default=60.0)
    ap.add_argument(
        "--recover-incomplete-renames",
        action="store_true",
        help=(
            "roll forward rename intents without a ready marker; use only "
            "offline or before the FUSE mount starts"
        ),
    )
    args = ap.parse_args()

    args.root.mkdir(parents=True, exist_ok=True)
    args.state_root.mkdir(parents=True, exist_ok=True)
    args.namespace_root.mkdir(parents=True, exist_ok=True)
    args.rename_root.mkdir(parents=True, exist_ok=True)
    args.socket.parent.mkdir(parents=True, exist_ok=True)
    with contextlib.suppress(FileNotFoundError):
        args.socket.unlink()

    cp = Checkpointer(
        args.root,
        args.state_root,
        args.namespace_root,
        args.rename_root,
        args.source_prefix,
        args.batch_max_files,
        args.batch_delay,
        args.durability,
        args.settle_delay,
    )

    # Resolve incomplete namespace transactions before any ordinary overlay
    # scan can mistake their old/new path state for an orphan.
    rename_recovery = cp.recover_pending_renames(
        include_incomplete=args.recover_incomplete_renames,
    )
    startup_seen, startup_dirty, startup_pruned = cp.scan_existing()

    thread = threading.Thread(
        target=cp.batch_worker,
        name="checkpoint-batch",
        daemon=True,
    )
    thread.start()

    print(
        f"checkpoint daemon ready socket={args.socket} root={args.root} "
        f"mode=batch batch_max_files={args.batch_max_files} "
        f"batch_delay={args.batch_delay} settle_delay={args.settle_delay} "
        f"durability={args.durability} "
        f"rename_recovery={rename_recovery} "
        f"startup_seen={startup_seen} startup_dirty={startup_dirty} "
        f"startup_pruned={startup_pruned}",
        flush=True,
    )

    sock = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 1024 * 1024)
    sock.bind(str(args.socket))
    os.chmod(args.socket, 0o600)
    sock.settimeout(1.0)

    stopping = False

    def stop(_sig, _frame):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)

    next_scan = time.monotonic() + args.scan_interval
    try:
        while not stopping:
            now = time.monotonic()
            if now >= next_scan:
                seen, dirty, pruned = cp.scan_existing()
                if dirty or pruned:
                    print(
                        f"periodic_scan seen={seen} dirty={dirty} pruned={pruned}",
                        flush=True,
                    )
                next_scan = now + args.scan_interval

            try:
                data = sock.recv(4096)
            except TimeoutError:
                continue
            except InterruptedError:
                continue

            try:
                source = data.decode(errors="strict")
            except UnicodeDecodeError:
                continue
            cp.enqueue(source, changed_event=True)
    finally:
        cp.stopping.set()
        sock.close()
        with contextlib.suppress(FileNotFoundError):
            args.socket.unlink()
        cp.q.put(None)
        thread.join(timeout=10.0)
        cp.close()

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
