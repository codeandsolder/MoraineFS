from __future__ import annotations

import importlib.util
import os
import shutil
import sys
import threading
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
MODULE_PATH = REPO_ROOT / "tools" / "checkpoint.py"
TEST_BASE = Path(
    os.environ.get("MORAINEFS_TEST_TMP", "/dev/shm/morainefs-tests/checkpoint-recovery")
)

spec = importlib.util.spec_from_file_location("io_tier_checkpoint", MODULE_PATH)
assert spec and spec.loader
mod = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = mod
spec.loader.exec_module(mod)

# State-machine tests don't need to force the whole backing filesystem.
mod.syncfs_fd = lambda _fd: None


def write(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)


def make_cp(case: Path, *, durability: str = "syncfs"):
    canonical = case / "canonical"
    canonical.mkdir(parents=True)
    prefix = str(canonical) + "/"
    cp = mod.Checkpointer(
        case / "overlay",
        case / "state",
        case / "namespace",
        case / "rename",
        prefix,
        64,
        0.01,
        durability,
    )
    for p in (cp.root, cp.state_root, cp.namespace_root, cp.rename_root):
        p.mkdir(parents=True, exist_ok=True)
    return cp, canonical, prefix


def source(prefix: str, name: str) -> str:
    return prefix + name


def marker(cp, old: str, new: str, *, ready: bool = False) -> None:
    p = cp.rename_marker_path(new)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(os.fsencode(old))
    if ready:
        cp.rename_ready_path(new).write_text("ready-v1\n")


def create_marker(cp, src: str) -> None:
    p = cp.marker_path(src)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text("created-v1\n")


def assert_common(cp, old: str, new: str) -> None:
    assert not cp.overlay_path(old).exists()
    assert cp.overlay_path(new).read_bytes() == b"authoritative-overlay"
    assert not os.path.lexists(old)
    assert os.path.lexists(new)
    assert cp.rename_marker_exists(new)
    assert cp.rename_ready_exists(new)


def case_before_overlay_move() -> None:
    cp, _canonical, prefix = make_cp(TEST_BASE / "before-overlay")
    old, new = source(prefix, "tmp"), source(prefix, "final")
    write(Path(old), b"old-canonical")
    write(Path(new), b"old-destination")
    write(cp.overlay_path(old), b"authoritative-overlay")
    create_marker(cp, old)
    marker(cp, old, new)
    assert cp.recover_pending_renames(include_incomplete=True) == {"recovered_incomplete": 1}
    assert_common(cp, old, new)
    assert cp.marker_exists(old)
    assert Path(new).read_bytes() == b"old-canonical"

    # Recovery leaves intent+ready until the authoritative overlay is copied
    # and the normal post-syncfs finalization path commits a clean generation.
    status, prepared = cp.prepare_copy(new, 0)
    assert status == "prepared" and prepared is not None
    assert Path(new).read_bytes() == b"authoritative-overlay"
    cp.write_state(new, prepared.clean_gen)
    write(cp.rename_dest_backup_path(new), b"stale-replaced-destination")
    cp.finalize_rename(new)
    assert not cp.rename_dest_backup_path(new).exists()
    assert not cp.rename_marker_exists(new)
    assert not cp.rename_ready_exists(new)
    assert not cp.marker_exists(old)
    cp.close()


def case_after_overlay_move() -> None:
    cp, _canonical, prefix = make_cp(TEST_BASE / "after-overlay")
    old, new = source(prefix, "tmp"), source(prefix, "final")
    write(Path(old), b"old-canonical")
    write(Path(new), b"old-destination")
    write(cp.overlay_path(new), b"authoritative-overlay")
    marker(cp, old, new)
    assert cp.recover_pending_renames(include_incomplete=True) == {"recovered_incomplete": 1}
    assert_common(cp, old, new)
    cp.close()


def case_dirty_destination_backup_before_source_move() -> None:
    cp, _canonical, prefix = make_cp(TEST_BASE / "dirty-dest-before-source")
    old, new = source(prefix, "tmp"), source(prefix, "final")
    write(Path(old), b"old-canonical")
    write(Path(new), b"old-destination-canonical")
    write(cp.overlay_path(old), b"authoritative-overlay")
    write(cp.rename_dest_backup_path(new), b"old-destination-overlay")
    marker(cp, old, new)
    assert cp.recover_pending_renames(include_incomplete=True) == {"recovered_incomplete": 1}
    assert_common(cp, old, new)
    assert not cp.rename_dest_backup_path(new).exists()
    cp.close()


def case_dirty_destination_backup_after_source_move() -> None:
    cp, _canonical, prefix = make_cp(TEST_BASE / "dirty-dest-after-source")
    old, new = source(prefix, "tmp"), source(prefix, "final")
    write(Path(old), b"old-canonical")
    write(Path(new), b"old-destination-canonical")
    write(cp.overlay_path(new), b"authoritative-overlay")
    write(cp.rename_dest_backup_path(new), b"old-destination-overlay")
    marker(cp, old, new)
    assert cp.recover_pending_renames(include_incomplete=True) == {"recovered_incomplete": 1}
    assert_common(cp, old, new)
    assert not cp.rename_dest_backup_path(new).exists()
    cp.close()


def case_after_canonical_move() -> None:
    cp, _canonical, prefix = make_cp(TEST_BASE / "after-canonical")
    old, new = source(prefix, "tmp"), source(prefix, "final")
    write(Path(new), b"old-canonical")
    write(cp.overlay_path(new), b"authoritative-overlay")
    marker(cp, old, new)
    assert cp.recover_pending_renames(include_incomplete=True) == {"recovered_incomplete": 1}
    assert_common(cp, old, new)
    cp.close()


def case_missing_canonical() -> None:
    cp, _canonical, prefix = make_cp(TEST_BASE / "missing-canonical")
    old, new = source(prefix, "tmp"), source(prefix, "final")
    write(cp.overlay_path(new), b"authoritative-overlay")
    marker(cp, old, new)
    assert cp.recover_pending_renames(include_incomplete=True) == {"recovered_incomplete": 1}
    assert_common(cp, old, new)
    assert Path(new).read_bytes() == b""
    cp.close()


def case_missing_overlay_stays_pending() -> None:
    cp, _canonical, prefix = make_cp(TEST_BASE / "missing-overlay")
    old, new = source(prefix, "tmp"), source(prefix, "final")
    write(Path(old), b"old-canonical")
    marker(cp, old, new)
    assert cp.recover_pending_renames(include_incomplete=True) == {"missing_overlay": 1}
    assert cp.rename_marker_exists(new)
    assert Path(old).exists()
    cp.close()


def case_deleted_completed_transaction_is_pruned() -> None:
    cp, _canonical, prefix = make_cp(TEST_BASE / "deleted-completed")
    old, new = source(prefix, "tmp"), source(prefix, "final")
    create_marker(cp, old)
    create_marker(cp, new)
    marker(cp, old, new, ready=True)
    assert cp.recover_pending_renames() == {"deleted_transaction": 1}
    assert not cp.rename_marker_exists(new)
    assert not cp.rename_ready_exists(new)
    assert not cp.marker_exists(old)
    assert not cp.marker_exists(new)
    cp.close()


def case_orphan_rename_aux_is_pruned() -> None:
    cp, _canonical, prefix = make_cp(TEST_BASE / "orphan-aux")
    new = source(prefix, "final")
    write(cp.rename_ready_path(new), b"ready-v1\n")
    write(cp.rename_dest_backup_path(new), b"orphaned-old-destination")
    counts = cp.recover_pending_renames()
    assert counts == {"orphan_ready_pruned": 1, "orphan_backup_pruned": 1}
    assert not cp.rename_ready_exists(new)
    assert not cp.rename_dest_backup_path(new).exists()
    cp.close()


def case_normal_checkpoint_finalizes_marker() -> None:
    cp, _canonical, prefix = make_cp(TEST_BASE / "checkpoint-finalize")
    old, new = source(prefix, "tmp"), source(prefix, "final")
    write(Path(new), b"stale-canonical")
    write(cp.overlay_path(new), b"authoritative-overlay")
    create_marker(cp, old)
    marker(cp, old, new, ready=True)
    status, prepared = cp.prepare_copy(new, 0)
    assert status == "prepared" and prepared is not None
    assert Path(new).read_bytes() == b"authoritative-overlay"
    cp.write_state(new, prepared.clean_gen)
    cp.finalize_rename(new)
    assert not cp.rename_marker_exists(new)
    assert not cp.rename_ready_exists(new)
    assert not cp.marker_exists(old)
    assert cp.read_state(new) == prepared.clean_gen
    cp.close()


def case_intent_without_ready_is_not_checkpointed() -> None:
    cp, _canonical, prefix = make_cp(TEST_BASE / "intent-not-ready")
    old, new = source(prefix, "tmp"), source(prefix, "final")
    write(Path(new), b"destination")
    write(cp.overlay_path(new), b"authoritative-overlay")
    marker(cp, old, new, ready=False)
    status, prepared = cp.prepare_copy(new, 0)
    assert status == "rename_in_progress" and prepared is None
    seen, dirty, _pruned = cp.scan_existing()
    assert seen == 1 and dirty == 0
    cp.close()


def case_file_durability_worker_commits_rename() -> None:
    cp, _canonical, prefix = make_cp(
        TEST_BASE / "file-durability-worker",
        durability="file",
    )
    old = source(prefix, "old-dir/tmp")
    new = source(prefix, "new-dir/final")
    Path(old).parent.mkdir(parents=True, exist_ok=True)
    write(Path(new), b"stale-canonical")
    write(cp.overlay_path(new), b"authoritative-overlay")
    create_marker(cp, old)
    marker(cp, old, new, ready=True)

    worker = threading.Thread(target=cp.batch_worker, daemon=True)
    worker.start()
    cp.enqueue(new, changed_event=False)
    cp.q.join()

    assert Path(new).read_bytes() == b"authoritative-overlay"
    assert cp.read_state(new) == cp.overlay_generation(new)
    assert not cp.rename_marker_exists(new)
    assert not cp.rename_ready_exists(new)
    assert not cp.marker_exists(old)

    cp.stopping.set()
    cp.q.put(None)
    worker.join(timeout=2)
    assert not worker.is_alive()
    cp.close()


def case_pending_event_race_requeues() -> None:
    cp, _canonical, prefix = make_cp(TEST_BASE / "pending-race")
    src = source(prefix, "file")
    cp.seq[src] = 0
    cp.pending.add(src)
    cp.seq[src] = 1
    cp.finish_source(src, retry=False, expected_seq=0)
    assert src in cp.pending
    assert cp.q.get_nowait() == src
    cp.q.task_done()

    # If completion wins the lock first, a later event must make a new item.
    cp.seq[src] = 1
    cp.pending.add(src)
    cp.finish_source(src, retry=False, expected_seq=1)
    assert src not in cp.pending
    cp.enqueue(src, changed_event=True)
    assert src in cp.pending
    assert cp.q.get_nowait() == src
    cp.q.task_done()
    cp.close()


def case_marker_preserves_odd_filename_bytes() -> None:
    cp, _canonical, prefix = make_cp(TEST_BASE / "odd-name")
    old = source(prefix, "tmp\nwith-space ")
    new = source(prefix, "final")
    marker(cp, old, new)
    assert cp.read_rename_marker(new) == old
    cp.close()


def main() -> None:
    shutil.rmtree(TEST_BASE, ignore_errors=True)
    TEST_BASE.mkdir(parents=True)
    tests = [
        case_before_overlay_move,
        case_after_overlay_move,
        case_dirty_destination_backup_before_source_move,
        case_dirty_destination_backup_after_source_move,
        case_after_canonical_move,
        case_missing_canonical,
        case_missing_overlay_stays_pending,
        case_deleted_completed_transaction_is_pruned,
        case_orphan_rename_aux_is_pruned,
        case_normal_checkpoint_finalizes_marker,
        case_intent_without_ready_is_not_checkpointed,
        case_file_durability_worker_commits_rename,
        case_pending_event_race_requeues,
        case_marker_preserves_odd_filename_bytes,
    ]
    for test in tests:
        test()
        print(f"PASS {test.__name__}")
    print(f"PASS all={len(tests)}")


if __name__ == "__main__":
    main()
