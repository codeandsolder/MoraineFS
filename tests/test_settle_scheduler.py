from __future__ import annotations

import importlib.util
import sys
import tempfile
import threading
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
MODULE = REPO_ROOT / "tools" / "checkpoint.py"
spec = importlib.util.spec_from_file_location("cp_settle", MODULE)
assert spec and spec.loader
mod = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = mod
spec.loader.exec_module(mod)
mod.syncfs_fd = lambda _fd: None


def make_cp(base: Path, settle: float):
    can = base / "canonical"
    can.mkdir(parents=True)
    cp = mod.Checkpointer(
        base / "overlay",
        base / "state",
        base / "namespace",
        base / "rename",
        str(can) + "/",
        16,
        0.01,
        "syncfs",
        settle,
    )
    for p in (cp.root, cp.state_root, cp.namespace_root, cp.rename_root):
        p.mkdir(parents=True, exist_ok=True)
    return cp, can


def drain_one(cp):
    item = cp.q.get(timeout=0.5)
    cp.q.task_done()
    return item


with tempfile.TemporaryDirectory(prefix="io-tier-settle-", dir="/dev/shm") as td:
    base = Path(td)

    # 1. A fresh changed event is not immediately eligible.
    cp, can = make_cp(base / "fresh", 0.15)
    src = str(can / "x")
    cp.enqueue(src, changed_event=True)
    assert drain_one(cp) == src
    assert cp._defer_if_unsettled(src)
    assert not cp.q.qsize()
    time.sleep(0.17)
    cp._promote_due_deferred()
    assert drain_one(cp) == src
    cp.close()
    print("PASS fresh_event_waits_for_quiet_age")

    # 2. A second event extends the same pending source's actual deadline.
    cp, can = make_cp(base / "extend", 0.18)
    src = str(can / "x")
    cp.enqueue(src, changed_event=True)
    assert drain_one(cp) == src
    assert cp._defer_if_unsettled(src)
    time.sleep(0.11)
    cp.enqueue(src, changed_event=True)  # remains pending; only deadline moves
    time.sleep(0.09)  # past original deadline, before new one
    cp._promote_due_deferred()
    assert cp.q.empty(), "original deadline was not invalidated"
    time.sleep(0.11)
    cp._promote_due_deferred()
    assert drain_one(cp) == src
    cp.close()
    print("PASS later_event_extends_deadline")

    # 3. Full worker: delete overlay before settle => never creates canonical.
    cp, can = make_cp(base / "delete", 0.15)
    src = str(can / "gone")
    ov = cp.overlay_path(src)
    ov.parent.mkdir(parents=True, exist_ok=True)
    ov.write_bytes(b"temporary")
    marker = cp.marker_path(src)
    marker.parent.mkdir(parents=True, exist_ok=True)
    marker.write_text("created-v1\n")
    worker = threading.Thread(target=cp.batch_worker, daemon=True)
    worker.start()
    cp.enqueue(src, changed_event=True)
    time.sleep(0.05)
    ov.unlink()
    marker.unlink()
    time.sleep(0.18)
    # allow due promotion + worker processing
    deadline = time.monotonic() + 0.5
    while time.monotonic() < deadline and src in cp.pending:
        time.sleep(0.01)
    assert not Path(src).exists()
    assert not ov.exists()
    cp.stopping.set()
    cp.q.put(None)
    worker.join(timeout=1)
    cp.close()
    print("PASS delete_before_settle_never_materializes")

print("PASS settle_scheduler_all=3")
