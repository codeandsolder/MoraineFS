#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::{CheckpointConfig, Checkpointer, Durability, PrepareStatus, Work};
use crate::journal::{FileNamespaceJournal, NamespaceJournal};
use crate::paths::Layout;
use crate::store::{FileMetadataStore, Generation, MetadataStore};

struct Fixture {
    _temp: TempDir,
    cp: Arc<Checkpointer>,
    layout: Layout,
    journal_root: PathBuf,
}

impl Fixture {
    fn new(durability: Durability) -> Self {
        Self::new_with_settle(durability, Duration::ZERO)
    }

    fn new_with_settle(durability: Durability, settle_delay: Duration) -> Self {
        let temp = tempfile::Builder::new()
            .prefix("moraine-checkpoint-")
            .tempdir_in("/dev/shm")
            .or_else(|_| TempDir::new())
            .unwrap();
        let canonical = temp.path().join("canonical");
        fs::create_dir_all(&canonical).unwrap();
        let layout = Layout {
            overlay_root: temp.path().join("overlay"),
            source_root: canonical,
        };
        let metadata_root = temp.path().join("metadata");
        let journal_root = temp.path().join("journal");
        for root in [&layout.overlay_root, &metadata_root, &journal_root] {
            fs::create_dir_all(root).unwrap();
        }
        let store: Arc<dyn MetadataStore> =
            Arc::new(FileMetadataStore::new(layout.clone(), metadata_root));
        let journal: Arc<dyn NamespaceJournal> = Arc::new(FileNamespaceJournal::new(
            layout.clone(),
            journal_root.clone(),
        ));
        let cp = Arc::new(
            Checkpointer::new(
                layout.clone(),
                store,
                journal,
                CheckpointConfig {
                    batch_max_files: 64,
                    batch_delay: Duration::from_millis(10),
                    settle_delay,
                    durability,
                },
            )
            .unwrap(),
        );
        Self {
            _temp: temp,
            cp,
            layout,
            journal_root,
        }
    }

    fn source(&self, name: impl AsRef<Path>) -> PathBuf {
        self.layout.source_root.join(name)
    }

    fn write(path: &Path, data: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, data).unwrap();
    }

    fn writeback(&self, source: &Path, data: &[u8]) {
        Self::write(&self.layout.overlay_path(source).unwrap(), data);
    }

    fn journal_path(&self, source: &Path, suffix: &[u8]) -> PathBuf {
        self.layout
            .adapter_path(&self.journal_root, source, suffix)
            .unwrap()
    }

    fn rename_backup_path(&self, source: &Path) -> PathBuf {
        self.layout.rename_backup_path(source).unwrap()
    }

    fn create_marker(&self, source: &Path) {
        Self::write(&self.journal_path(source, b".created"), b"created-v1\n");
    }

    fn rename_marker(&self, old: &Path, new: &Path, ready: bool) {
        Self::write(
            &self.journal_path(new, b".rename"),
            old.as_os_str().as_bytes(),
        );
        if ready {
            Self::write(&self.journal_path(new, b".rename.ready"), b"ready-v1\n");
        }
    }

    fn assert_common(&self, old: &Path, new: &Path) {
        assert!(!self.layout.overlay_path(old).unwrap().exists());
        assert_eq!(
            fs::read(self.layout.overlay_path(new).unwrap()).unwrap(),
            b"authoritative-overlay"
        );
        assert!(fs::symlink_metadata(old).is_err());
        assert!(fs::symlink_metadata(new).is_ok());
        assert!(self.cp.journal.rename_marker_exists(new).unwrap());
        assert!(self.cp.journal.rename_ready_exists(new).unwrap());
    }
}

fn one(key: &str) -> HashMap<String, u64> {
    HashMap::from([(key.to_owned(), 1)])
}

#[test]
fn recovery_before_overlay_move_rolls_forward_then_finalizes() {
    let f = Fixture::new(Durability::File);
    let old = f.source("tmp");
    let new = f.source("final");
    Fixture::write(&old, b"old-canonical");
    Fixture::write(&new, b"old-destination");
    f.writeback(&old, b"authoritative-overlay");
    f.create_marker(&old);
    f.rename_marker(&old, &new, false);
    assert_eq!(
        f.cp.recover_pending_renames(true).unwrap(),
        one("recovered_incomplete")
    );
    f.assert_common(&old, &new);
    assert!(f.cp.journal.create_marker_exists(&old).unwrap());
    assert_eq!(fs::read(&new).unwrap(), b"old-canonical");

    let (status, prepared) = f.cp.prepare_copy(&new, 0);
    assert_eq!(status, PrepareStatus::Prepared);
    let prepared = prepared.unwrap();
    assert_eq!(fs::read(&new).unwrap(), b"authoritative-overlay");
    f.cp.store
        .write_generation(&new, prepared.clean_gen)
        .unwrap();
    Fixture::write(&f.rename_backup_path(&new), b"stale-destination");
    f.cp.finalize_rename(&new).unwrap();
    assert!(!f.rename_backup_path(&new).exists());
    assert!(!f.cp.journal.rename_marker_exists(&new).unwrap());
    assert!(!f.cp.journal.rename_ready_exists(&new).unwrap());
    assert!(!f.cp.journal.create_marker_exists(&old).unwrap());
}

#[test]
fn recovery_after_overlay_move_rolls_forward() {
    let f = Fixture::new(Durability::File);
    let old = f.source("tmp");
    let new = f.source("final");
    Fixture::write(&old, b"old-canonical");
    Fixture::write(&new, b"old-destination");
    f.writeback(&new, b"authoritative-overlay");
    f.rename_marker(&old, &new, false);
    assert_eq!(
        f.cp.recover_pending_renames(true).unwrap(),
        one("recovered_incomplete")
    );
    f.assert_common(&old, &new);
}

#[test]
fn recovery_removes_dirty_destination_backup_before_source_move() {
    let f = Fixture::new(Durability::File);
    let old = f.source("tmp");
    let new = f.source("final");
    Fixture::write(&old, b"old-canonical");
    Fixture::write(&new, b"old-destination-canonical");
    f.writeback(&old, b"authoritative-overlay");
    Fixture::write(&f.rename_backup_path(&new), b"old-destination-overlay");
    f.rename_marker(&old, &new, false);
    assert_eq!(
        f.cp.recover_pending_renames(true).unwrap(),
        one("recovered_incomplete")
    );
    f.assert_common(&old, &new);
    assert!(!f.rename_backup_path(&new).exists());
}

#[test]
fn recovery_removes_dirty_destination_backup_after_source_move() {
    let f = Fixture::new(Durability::File);
    let old = f.source("tmp");
    let new = f.source("final");
    Fixture::write(&old, b"old-canonical");
    Fixture::write(&new, b"old-destination-canonical");
    f.writeback(&new, b"authoritative-overlay");
    Fixture::write(&f.rename_backup_path(&new), b"old-destination-overlay");
    f.rename_marker(&old, &new, false);
    assert_eq!(
        f.cp.recover_pending_renames(true).unwrap(),
        one("recovered_incomplete")
    );
    f.assert_common(&old, &new);
    assert!(!f.rename_backup_path(&new).exists());
}

#[test]
fn recovery_after_canonical_move_preserves_authoritative_overlay() {
    let f = Fixture::new(Durability::File);
    let old = f.source("tmp");
    let new = f.source("final");
    Fixture::write(&new, b"old-canonical");
    f.writeback(&new, b"authoritative-overlay");
    f.rename_marker(&old, &new, false);
    assert_eq!(
        f.cp.recover_pending_renames(true).unwrap(),
        one("recovered_incomplete")
    );
    f.assert_common(&old, &new);
}

#[test]
fn recovery_recreates_missing_canonical_destination() {
    let f = Fixture::new(Durability::File);
    let old = f.source("tmp");
    let new = f.source("final");
    f.writeback(&new, b"authoritative-overlay");
    f.rename_marker(&old, &new, false);
    assert_eq!(
        f.cp.recover_pending_renames(true).unwrap(),
        one("recovered_incomplete")
    );
    f.assert_common(&old, &new);
    assert_eq!(fs::read(&new).unwrap(), Vec::<u8>::new());
}

#[test]
fn recovery_leaves_missing_overlay_pending() {
    let f = Fixture::new(Durability::File);
    let old = f.source("tmp");
    let new = f.source("final");
    Fixture::write(&old, b"old-canonical");
    f.rename_marker(&old, &new, false);
    assert_eq!(
        f.cp.recover_pending_renames(true).unwrap(),
        one("missing_overlay")
    );
    assert!(f.cp.journal.rename_marker_exists(&new).unwrap());
    assert!(old.exists());
}

#[test]
fn deleted_completed_transaction_is_pruned() {
    let f = Fixture::new(Durability::File);
    let old = f.source("tmp");
    let new = f.source("final");
    f.create_marker(&old);
    f.create_marker(&new);
    f.rename_marker(&old, &new, true);
    assert_eq!(
        f.cp.recover_pending_renames(false).unwrap(),
        one("deleted_transaction")
    );
    assert!(!f.cp.journal.rename_marker_exists(&new).unwrap());
    assert!(!f.cp.journal.rename_ready_exists(&new).unwrap());
    assert!(!f.cp.journal.create_marker_exists(&old).unwrap());
    assert!(!f.cp.journal.create_marker_exists(&new).unwrap());
}

#[test]
fn orphan_rename_auxiliaries_are_pruned() {
    let f = Fixture::new(Durability::File);
    let new = f.source("final");
    Fixture::write(&f.journal_path(&new, b".rename.ready"), b"ready-v1\n");
    Fixture::write(&f.rename_backup_path(&new), b"orphan-destination");
    assert_eq!(
        f.cp.recover_pending_renames(false).unwrap(),
        HashMap::from([
            ("orphan_ready_pruned".to_owned(), 1),
            ("orphan_backup_pruned".to_owned(), 1),
        ])
    );
    assert!(!f.cp.journal.rename_ready_exists(&new).unwrap());
    assert!(!f.rename_backup_path(&new).exists());
}

#[test]
fn normal_checkpoint_finalizes_rename_marker() {
    let f = Fixture::new(Durability::File);
    let old = f.source("tmp");
    let new = f.source("final");
    Fixture::write(&new, b"stale-canonical");
    f.writeback(&new, b"authoritative-overlay");
    f.create_marker(&old);
    f.rename_marker(&old, &new, true);
    let (status, prepared) = f.cp.prepare_copy(&new, 0);
    assert_eq!(status, PrepareStatus::Prepared);
    let prepared = prepared.unwrap();
    f.cp.store
        .write_generation(&new, prepared.clean_gen)
        .unwrap();
    f.cp.finalize_rename(&new).unwrap();
    assert_eq!(fs::read(&new).unwrap(), b"authoritative-overlay");
    assert!(!f.cp.journal.rename_marker_exists(&new).unwrap());
    assert!(!f.cp.journal.rename_ready_exists(&new).unwrap());
    assert!(!f.cp.journal.create_marker_exists(&old).unwrap());
    assert_eq!(
        f.cp.store.read_generation(&new).unwrap(),
        Some(prepared.clean_gen)
    );
}

#[test]
fn rename_intent_without_ready_is_not_checkpointed() {
    let f = Fixture::new(Durability::File);
    let old = f.source("tmp");
    let new = f.source("final");
    Fixture::write(&new, b"destination");
    f.writeback(&new, b"authoritative-overlay");
    f.rename_marker(&old, &new, false);
    let (status, prepared) = f.cp.prepare_copy(&new, 0);
    assert_eq!(status, PrepareStatus::RenameInProgress);
    assert!(prepared.is_none());
    let (seen, dirty, _) = f.cp.scan_existing().unwrap();
    assert_eq!((seen, dirty), (1, 0));
}

#[test]
fn file_durability_worker_commits_rename() {
    let f = Fixture::new(Durability::File);
    let old = f.source("old-dir/tmp");
    let new = f.source("new-dir/final");
    fs::create_dir_all(old.parent().unwrap()).unwrap();
    Fixture::write(&new, b"stale-canonical");
    f.writeback(&new, b"authoritative-overlay");
    f.create_marker(&old);
    f.rename_marker(&old, &new, true);
    let cp = Arc::clone(&f.cp);
    let worker = thread::spawn(move || cp.run_worker());
    f.cp.enqueue(&new, false).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let committed = f.cp.store.read_generation(&new).unwrap()
            == f.cp.overlay_generation(&new).unwrap()
            && !f.cp.journal.rename_marker_exists(&new).unwrap();
        if committed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "checkpoint worker did not commit in time"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(fs::read(&new).unwrap(), b"authoritative-overlay");
    assert!(!f.cp.journal.rename_ready_exists(&new).unwrap());
    assert!(!f.cp.journal.create_marker_exists(&old).unwrap());
    f.cp.stop();
    worker.join().unwrap().unwrap();
}

#[test]
fn pending_event_race_requeues() {
    let f = Fixture::new(Durability::File);
    let source = f.source("file");
    {
        let mut state = f.cp.lock_scheduler().unwrap();
        state.seq.insert(source.clone(), 0);
        state.pending.insert(source.clone());
        state.seq.insert(source.clone(), 1);
    }
    f.cp.finish_source(&source, false, Some(0)).unwrap();
    assert!(f.cp.lock_scheduler().unwrap().pending.contains(&source));
    assert!(matches!(
        f.cp.receiver.recv_timeout(Duration::from_millis(50)).unwrap(),
        Work::Source(ref queued) if queued == &source
    ));

    {
        let mut state = f.cp.lock_scheduler().unwrap();
        state.seq.insert(source.clone(), 1);
        state.pending.insert(source.clone());
    }
    f.cp.finish_source(&source, false, Some(1)).unwrap();
    assert!(!f.cp.lock_scheduler().unwrap().pending.contains(&source));
    f.cp.enqueue(&source, true).unwrap();
    assert!(f.cp.lock_scheduler().unwrap().pending.contains(&source));
    assert!(matches!(
        f.cp.receiver.recv_timeout(Duration::from_millis(50)).unwrap(),
        Work::Source(ref queued) if queued == &source
    ));
}

#[test]
fn rename_marker_preserves_odd_filename_bytes() {
    let f = Fixture::new(Durability::File);
    let mut raw = b"tmp\nwith-space ".to_vec();
    raw.push(0xFF);
    let old = f.source(PathBuf::from(OsString::from_vec(raw)));
    let new = f.source("final");
    f.rename_marker(&old, &new, false);
    assert_eq!(
        f.cp.journal
            .read_rename_source(&new)
            .unwrap()
            .unwrap()
            .as_os_str(),
        old.as_os_str()
    );
}

#[test]
fn fresh_event_waits_for_quiet_age() {
    let f = Fixture::new_with_settle(Durability::File, Duration::from_millis(150));
    let source = f.source("file");
    f.cp.enqueue(&source, true).unwrap();
    assert!(matches!(
        f.cp.receiver.recv_timeout(Duration::from_millis(50)).unwrap(),
        Work::Source(ref queued) if queued == &source
    ));
    assert!(f.cp.defer_if_unsettled(&source).unwrap());
    assert!(f.cp.receiver.try_recv().is_err());
    thread::sleep(Duration::from_millis(170));
    f.cp.promote_due_deferred().unwrap();
    assert!(matches!(
        f.cp.receiver.recv_timeout(Duration::from_millis(50)).unwrap(),
        Work::Source(ref queued) if queued == &source
    ));
}

#[test]
fn later_event_extends_settle_deadline() {
    let f = Fixture::new_with_settle(Durability::File, Duration::from_millis(180));
    let source = f.source("file");
    f.cp.enqueue(&source, true).unwrap();
    assert!(matches!(
        f.cp.receiver.recv_timeout(Duration::from_millis(50)).unwrap(),
        Work::Source(ref queued) if queued == &source
    ));
    assert!(f.cp.defer_if_unsettled(&source).unwrap());
    thread::sleep(Duration::from_millis(110));
    assert!(!f.cp.enqueue(&source, true).unwrap());
    thread::sleep(Duration::from_millis(90));
    f.cp.promote_due_deferred().unwrap();
    assert!(f.cp.receiver.try_recv().is_err());
    thread::sleep(Duration::from_millis(110));
    f.cp.promote_due_deferred().unwrap();
    assert!(matches!(
        f.cp.receiver.recv_timeout(Duration::from_millis(50)).unwrap(),
        Work::Source(ref queued) if queued == &source
    ));
}

#[test]
fn delete_before_settle_never_materializes() {
    let f = Fixture::new_with_settle(Durability::File, Duration::from_millis(150));
    let source = f.source("gone");
    let overlay = f.layout.overlay_path(&source).unwrap();
    let marker = f.journal_path(&source, b".created");
    Fixture::write(&overlay, b"temporary");
    Fixture::write(&marker, b"created-v1\n");
    let cp = Arc::clone(&f.cp);
    let worker = thread::spawn(move || cp.run_worker());
    f.cp.enqueue(&source, true).unwrap();
    thread::sleep(Duration::from_millis(50));
    fs::remove_file(&overlay).unwrap();
    fs::remove_file(&marker).unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while f.cp.lock_scheduler().unwrap().pending.contains(&source) {
        assert!(
            Instant::now() < deadline,
            "settled deletion remained pending"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!source.exists());
    assert!(!overlay.exists());
    f.cp.stop();
    worker.join().unwrap().unwrap();
}

#[test]
fn deleted_before_checkpoint_never_materializes() {
    let f = Fixture::new(Durability::File);
    let source = f.source("file");
    f.writeback(&source, b"transient");
    fs::remove_file(f.layout.overlay_path(&source).unwrap()).unwrap();
    let (status, prepared) = f.cp.prepare_copy(&source, 0);
    assert_eq!(status, PrepareStatus::OverlayMissing);
    assert!(prepared.is_none());
    assert!(!source.exists());
}

#[test]
fn file_state_adapter_preserves_non_utf8_source_names() -> io::Result<()> {
    let f = Fixture::new(Durability::File);
    let source = f.source(PathBuf::from(OsString::from_vec(vec![b'f', 0xFF])));
    let generation = Generation {
        size: 7,
        mtime_ns: 11,
        ctime_ns: 13,
    };
    f.cp.store.write_generation(&source, generation)?;
    assert_eq!(f.cp.store.read_generation(&source)?, Some(generation));
    Ok(())
}
