use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, ErrorKind};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};
use rustix::fs::{
    Gid, Mode, OFlags, Timespec, Timestamps, Uid, fchmod, fchown, futimens, open, syncfs,
};
use walkdir::WalkDir;

use crate::journal::NamespaceJournal;
use crate::paths::Layout;
use crate::store::{Generation, MetadataStore, remove_if_exists, sync_directory};

const COPY_CHUNK: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    SyncFs,
    File,
}

#[derive(Debug, Clone)]
pub struct CheckpointConfig {
    pub batch_max_files: usize,
    pub batch_delay: Duration,
    pub settle_delay: Duration,
    pub durability: Durability,
}

#[derive(Debug)]
pub struct Prepared {
    pub source: PathBuf,
    pub start_seq: u64,
    pub clean_gen: Generation,
    pub copied: u64,
    pub copy_time: Duration,
    pub changed_during_copy: bool,
    pub canonical_file: Option<File>,
    pub sync_dirs: BTreeSet<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareStatus {
    Prepared,
    RenameInProgress,
    OverlayMissing,
    OverlayNotRegular,
    DiscardedDeleted,
    InvalidRenameMarker,
    ShortRead,
    ShortWrite,
    CanonicalOpen(i32),
    Io(i32),
}

impl PrepareStatus {
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Prepared => "prepared".to_owned(),
            Self::RenameInProgress => "rename_in_progress".to_owned(),
            Self::OverlayMissing => "overlay_missing".to_owned(),
            Self::OverlayNotRegular => "overlay_not_regular".to_owned(),
            Self::DiscardedDeleted => "discarded_deleted".to_owned(),
            Self::InvalidRenameMarker => "invalid_rename_marker".to_owned(),
            Self::ShortRead => "short_read".to_owned(),
            Self::ShortWrite => "short_write".to_owned(),
            Self::CanonicalOpen(errno) => format!("canonical_open_error:{errno}"),
            Self::Io(errno) => format!("io_error:{errno}"),
        }
    }
}

#[derive(Debug)]
enum Work {
    Source(PathBuf),
    Stop,
}

#[derive(Debug, Default)]
struct SchedulerState {
    seq: HashMap<PathBuf, u64>,
    pending: HashSet<PathBuf>,
    last_changed: HashMap<PathBuf, Instant>,
    deferred: BinaryHeap<Reverse<(Instant, u64, PathBuf)>>,
    deferred_serial: u64,
}

pub struct Checkpointer {
    layout: Layout,
    store: Arc<dyn MetadataStore>,
    journal: Arc<dyn NamespaceJournal>,
    config: CheckpointConfig,
    sync_file: File,
    sender: Sender<Work>,
    receiver: Receiver<Work>,
    scheduler: Mutex<SchedulerState>,
    stopping: AtomicBool,
}

impl Checkpointer {
    pub fn new(
        layout: Layout,
        store: Arc<dyn MetadataStore>,
        journal: Arc<dyn NamespaceJournal>,
        config: CheckpointConfig,
    ) -> io::Result<Self> {
        if config.batch_max_files == 0 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "batch_max_files must be non-zero",
            ));
        }
        let sync_file = File::open(&layout.source_prefix)?;
        let (sender, receiver) = unbounded();
        Ok(Self {
            layout,
            store,
            journal,
            config,
            sync_file,
            sender,
            receiver,
            scheduler: Mutex::new(SchedulerState::default()),
            stopping: AtomicBool::new(false),
        })
    }

    #[must_use]
    pub const fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        let _ = self.sender.send(Work::Stop);
    }

    pub fn enqueue(&self, source: &Path, changed_event: bool) -> io::Result<bool> {
        self.layout.relative_source(source)?;
        let now = Instant::now();
        let mut state = self.lock_scheduler()?;
        if changed_event {
            let seq = state.seq.entry(source.to_path_buf()).or_default();
            *seq = seq.saturating_add(1);
            state.last_changed.insert(source.to_path_buf(), now);
        } else {
            state.seq.entry(source.to_path_buf()).or_default();
            state
                .last_changed
                .entry(source.to_path_buf())
                .or_insert(now);
        }
        if !state.pending.insert(source.to_path_buf()) {
            return Ok(false);
        }
        drop(state);
        self.sender
            .send(Work::Source(source.to_path_buf()))
            .map_err(channel_closed)?;
        Ok(true)
    }

    fn lock_scheduler(&self) -> io::Result<std::sync::MutexGuard<'_, SchedulerState>> {
        self.scheduler
            .lock()
            .map_err(|_| io::Error::other("scheduler mutex poisoned"))
    }

    fn settle_due_locked(&self, state: &SchedulerState, source: &Path) -> Instant {
        state
            .last_changed
            .get(source)
            .copied()
            .unwrap_or_else(Instant::now)
            + self.config.settle_delay
    }

    fn defer_source(&self, source: PathBuf, due: Instant) -> io::Result<()> {
        let mut state = self.lock_scheduler()?;
        state.deferred_serial = state.deferred_serial.saturating_add(1);
        let serial = state.deferred_serial;
        state.deferred.push(Reverse((due, serial, source)));
        drop(state);
        Ok(())
    }

    fn defer_if_unsettled(&self, source: &Path) -> io::Result<bool> {
        if self.config.settle_delay.is_zero() {
            return Ok(false);
        }
        let due = {
            let state = self.lock_scheduler()?;
            self.settle_due_locked(&state, source)
        };
        if due <= Instant::now() {
            return Ok(false);
        }
        self.defer_source(source.to_path_buf(), due)?;
        Ok(true)
    }

    fn promote_due_deferred(&self) -> io::Result<()> {
        if self.config.settle_delay.is_zero() {
            return Ok(());
        }
        loop {
            let source = {
                let now = Instant::now();
                let mut state = self.lock_scheduler()?;
                let Some(Reverse((queued_due, _serial, source))) = state.deferred.peek().cloned()
                else {
                    return Ok(());
                };
                if queued_due > now {
                    return Ok(());
                }
                let _ = state.deferred.pop();
                let actual_due = self.settle_due_locked(&state, &source);
                if actual_due > now {
                    state.deferred_serial = state.deferred_serial.saturating_add(1);
                    let serial = state.deferred_serial;
                    state.deferred.push(Reverse((actual_due, serial, source)));
                    drop(state);
                    None
                } else {
                    drop(state);
                    Some(source)
                }
            };
            let Some(source) = source else {
                continue;
            };
            self.sender
                .send(Work::Source(source))
                .map_err(channel_closed)?;
        }
    }

    fn next_deferred_wait(&self, maximum: Duration) -> io::Result<Duration> {
        if self.config.settle_delay.is_zero() {
            return Ok(maximum);
        }
        let due = {
            let state = self.lock_scheduler()?;
            state.deferred.peek().map(|Reverse((due, _, _))| *due)
        };
        let Some(due) = due else {
            return Ok(maximum);
        };
        Ok(due.saturating_duration_since(Instant::now()).min(maximum))
    }

    pub fn prepare_copy(&self, source: &Path, start_seq: u64) -> (PrepareStatus, Option<Prepared>) {
        match self.prepare_copy_inner(source, start_seq) {
            Ok(Some(prepared)) => (PrepareStatus::Prepared, Some(prepared)),
            Ok(None) => (PrepareStatus::OverlayMissing, None),
            Err(PrepareError::Status(status)) => (status, None),
            Err(PrepareError::Io(error)) => (
                PrepareStatus::Io(error.raw_os_error().unwrap_or_default()),
                None,
            ),
        }
    }

    fn prepare_copy_inner(
        &self,
        source: &Path,
        start_seq: u64,
    ) -> Result<Option<Prepared>, PrepareError> {
        if self.journal.rename_marker_exists(source)?
            && !self.journal.rename_ready_exists(source)?
        {
            return Err(PrepareError::Status(PrepareStatus::RenameInProgress));
        }

        let overlay = self.layout.writeback_path(source)?;
        let overlay_file = match open_read_nofollow(&overlay) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(_) => return Ok(None),
        };
        let started = Instant::now();
        let before = overlay_file.metadata()?;
        if !before.file_type().is_file() {
            return Err(PrepareError::Status(PrepareStatus::OverlayNotRegular));
        }
        let before_gen = Generation::from_metadata(&before)?;

        let canonical_file = match self.open_or_recover_canonical(source, &before) {
            Ok(file) => file,
            Err(error) => {
                if error.kind() == ErrorKind::NotFound
                    && !self.journal.create_marker_exists(source)?
                {
                    self.discard_orphan(source)?;
                    return Err(PrepareError::Status(PrepareStatus::DiscardedDeleted));
                }
                return Err(PrepareError::Status(PrepareStatus::CanonicalOpen(
                    error.raw_os_error().unwrap_or_default(),
                )));
            }
        };

        let mut buffer = vec![0_u8; COPY_CHUNK];
        let mut offset = 0_u64;
        while offset < before_gen.size {
            let remaining = before_gen.size - offset;
            let amount = usize::try_from(remaining.min(COPY_CHUNK as u64))
                .map_err(|_| io::Error::other("copy size conversion failed"))?;
            let read = overlay_file.read_at(&mut buffer[..amount], offset)?;
            if read == 0 {
                return Err(PrepareError::Status(PrepareStatus::ShortRead));
            }
            let mut written = 0_usize;
            while written < read {
                let delta = canonical_file.write_at(
                    &buffer[written..read],
                    offset + u64::try_from(written).unwrap_or(0),
                )?;
                if delta == 0 {
                    return Err(PrepareError::Status(PrepareStatus::ShortWrite));
                }
                written += delta;
            }
            offset = offset.saturating_add(u64::try_from(read).unwrap_or(0));
        }
        canonical_file.set_len(before_gen.size)?;
        apply_metadata_best_effort(&canonical_file, &before);

        let mut sync_dirs = BTreeSet::new();
        if self.config.durability == Durability::File {
            if self.journal.create_marker_exists(source)?
                && let Some(parent) = source.parent()
            {
                sync_dirs.insert(parent.to_path_buf());
            }
            if self.journal.rename_marker_exists(source)? {
                if !self.journal.rename_ready_exists(source)? {
                    return Err(PrepareError::Status(PrepareStatus::RenameInProgress));
                }
                let Some(old) = self.journal.read_rename_source(source)? else {
                    return Err(PrepareError::Status(PrepareStatus::InvalidRenameMarker));
                };
                if let Some(parent) = old.parent() {
                    sync_dirs.insert(parent.to_path_buf());
                }
                if let Some(parent) = source.parent() {
                    sync_dirs.insert(parent.to_path_buf());
                }
            }
        }

        let after = overlay_file.metadata()?;
        let changed_during_copy = Generation::from_metadata(&after)? != before_gen;
        let keep_file = (self.config.durability == Durability::File).then_some(canonical_file);
        Ok(Some(Prepared {
            source: source.to_path_buf(),
            start_seq,
            clean_gen: before_gen,
            copied: before_gen.size,
            copy_time: started.elapsed(),
            changed_during_copy,
            canonical_file: keep_file,
            sync_dirs,
        }))
    }

    fn open_or_recover_canonical(
        &self,
        source: &Path,
        overlay_metadata: &fs::Metadata,
    ) -> io::Result<File> {
        match open_write_nofollow(source) {
            Ok(file) => return Ok(file),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let recoverable = self.journal.create_marker_exists(source)?
            || (self.journal.rename_marker_exists(source)?
                && self.journal.rename_ready_exists(source)?);
        if !recoverable {
            return Err(io::Error::from(ErrorKind::NotFound));
        }
        let file = create_write_nofollow(source, overlay_metadata.mode() & 0o7777)?;
        apply_owner_mode_best_effort(&file, overlay_metadata);
        Ok(file)
    }

    fn discard_orphan(&self, source: &Path) -> io::Result<()> {
        remove_if_exists(&self.layout.writeback_path(source)?)?;
        self.store.remove_generation(source)
    }

    pub fn overlay_generation(&self, source: &Path) -> io::Result<Option<Generation>> {
        let path = self.layout.writeback_path(source)?;
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_file() {
            return Ok(None);
        }
        Ok(Some(Generation::from_metadata(&metadata)?))
    }

    pub fn finalize_rename(&self, source: &Path) -> io::Result<()> {
        let old = match self.journal.read_rename_source(source)? {
            Some(old) => old,
            None if self.journal.rename_marker_exists(source)? => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "invalid rename marker",
                ));
            }
            None => return Ok(()),
        };

        let overlay = self.layout.writeback_path(source)?;
        if overlay.exists()
            && let Some(parent) = overlay.parent()
        {
            sync_directory(parent)?;
        }
        if self.journal.create_marker_exists(&old)? {
            self.journal.clear_create_marker(&old)?;
        }
        if self.journal.rename_ready_exists(source)? {
            self.journal.clear_rename_ready(source)?;
        }
        self.journal.remove_destination_backup(source)?;
        self.journal.clear_rename_marker(source)
    }

    pub fn recover_pending_renames(
        &self,
        include_incomplete: bool,
    ) -> io::Result<HashMap<String, u64>> {
        let mut counts = HashMap::new();
        let root = self.journal.rename_root();
        if !root.exists() {
            return Ok(counts);
        }

        let mut markers = collect_suffix_files(root, b".rename")?;
        markers.sort();
        for marker in markers {
            match self.recover_rename_marker(&marker, include_incomplete) {
                Ok(Some(status)) => bump(&mut counts, status),
                Ok(None) => {}
                Err(error) => {
                    let key = format!("error_{}", error.raw_os_error().unwrap_or_default());
                    bump(&mut counts, &key);
                }
            }
        }

        self.prune_orphan_rename_aux(&mut counts)?;
        Ok(counts)
    }

    fn recover_rename_marker(
        &self,
        marker: &Path,
        include_incomplete: bool,
    ) -> io::Result<Option<&'static str>> {
        let root = self.journal.rename_root();
        let Ok(destination) = source_from_marker(root, &self.layout, marker, b".rename") else {
            return Ok(None);
        };
        let Some(old) = self.journal.read_rename_source(&destination)? else {
            return Ok(Some("invalid_marker"));
        };
        let ready = self.journal.rename_ready_exists(&destination)?;
        if !ready && !include_incomplete {
            return Ok(Some("deferred_incomplete"));
        }

        let old_overlay = self.layout.writeback_path(&old)?;
        let new_overlay = self.layout.writeback_path(&destination)?;
        let old_exists = old_overlay.is_file();
        let new_exists = new_overlay.is_file();
        if old_exists {
            if let Some(parent) = new_overlay.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(&old_overlay, &new_overlay)?;
            if let Some(parent) = new_overlay.parent() {
                sync_directory(parent)?;
            }
            if old_overlay.parent() != new_overlay.parent()
                && let Some(parent) = old_overlay.parent()
            {
                sync_directory(parent)?;
            }
        } else if !new_exists {
            if !lexists(&old) && !lexists(&destination) {
                if self.journal.create_marker_exists(&old)? {
                    self.journal.clear_create_marker(&old)?;
                }
                if self.journal.create_marker_exists(&destination)? {
                    self.journal.clear_create_marker(&destination)?;
                }
                if self.journal.rename_ready_exists(&destination)? {
                    self.journal.clear_rename_ready(&destination)?;
                }
                self.journal.remove_destination_backup(&destination)?;
                self.journal.clear_rename_marker(&destination)?;
                return Ok(Some("deleted_transaction"));
            }
            return Ok(Some("missing_overlay"));
        }

        self.store.remove_generation(&old)?;
        self.store.remove_generation(&destination)?;
        if lexists(&old) {
            fs::rename(&old, &destination)?;
        } else if !lexists(&destination) {
            let metadata = fs::metadata(&new_overlay)?;
            let file = create_write_nofollow(&destination, metadata.mode() & 0o7777)?;
            apply_owner_mode_best_effort(&file, &metadata);
        }

        self.journal.remove_destination_backup(&destination)?;
        if ready {
            Ok(Some("ready_pending"))
        } else {
            self.journal.mark_rename_ready(&destination)?;
            Ok(Some("recovered_incomplete"))
        }
    }

    fn prune_orphan_rename_aux(&self, counts: &mut HashMap<String, u64>) -> io::Result<()> {
        let root = self.journal.rename_root();
        for (suffix, marker_suffix, key) in [
            (
                b".rename.ready".as_slice(),
                b".rename".as_slice(),
                "orphan_ready_pruned",
            ),
            (
                b".rename.dst-overlay".as_slice(),
                b".rename".as_slice(),
                "orphan_backup_pruned",
            ),
        ] {
            let mut paths = collect_suffix_files(root, suffix)?;
            paths.sort();
            for path in paths {
                let Some(name) = path.file_name() else {
                    continue;
                };
                let raw = name.as_bytes();
                let Some(base) = raw.strip_suffix(suffix) else {
                    continue;
                };
                let mut marker_name = base.to_vec();
                marker_name.extend_from_slice(marker_suffix);
                let marker = path.with_file_name(OsString::from_vec(marker_name));
                if marker.exists() || fs::remove_file(&path).is_err() {
                    continue;
                }
                if let Some(parent) = path.parent()
                    && sync_directory(parent).is_err()
                {
                    continue;
                }
                bump(counts, key);
            }
        }
        Ok(())
    }

    pub fn scan_existing(&self) -> io::Result<(u64, u64, u64)> {
        let mut seen = 0_u64;
        let mut dirty = 0_u64;
        if !self.layout.writeback_root.exists() {
            return Ok((0, 0, self.prune_states()?));
        }
        for entry in WalkDir::new(&self.layout.writeback_root)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            if entry
                .file_name()
                .as_bytes()
                .windows(b".io-tier-tmp.".len())
                .any(|w| w == b".io-tier-tmp.")
            {
                continue;
            }
            let Ok(relative) = entry.path().strip_prefix(&self.layout.writeback_root) else {
                continue;
            };
            let Ok(source) = self.layout.source_from_relative(relative) else {
                continue;
            };
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            seen = seen.saturating_add(1);
            let generation = Generation::from_metadata(&metadata)?;
            let clean = self.store.read_generation(&source)? == Some(generation);
            let created = self.journal.create_marker_exists(&source)?;
            let renamed = self.journal.rename_marker_exists(&source)?;
            let ready = self.journal.rename_ready_exists(&source)?;
            if renamed && !ready {
                continue;
            }
            if clean && !created && !renamed {
                continue;
            }
            let _ = self.enqueue(&source, false)?;
            dirty = dirty.saturating_add(1);
        }
        Ok((seen, dirty, self.prune_states()?))
    }

    pub fn prune_states(&self) -> io::Result<u64> {
        if !self.layout.state_root.exists() {
            return Ok(0);
        }
        let mut removed = 0_u64;
        for entry in WalkDir::new(&self.layout.state_root)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().as_bytes();
            let Some(base) = name.strip_suffix(b".state") else {
                continue;
            };
            let Ok(relative) = entry.path().strip_prefix(&self.layout.state_root) else {
                continue;
            };
            let overlay_relative = relative.with_file_name(OsString::from_vec(base.to_vec()));
            if self.layout.writeback_root.join(overlay_relative).exists() {
                continue;
            }
            if fs::remove_file(entry.path()).is_ok() {
                removed = removed.saturating_add(1);
            }
        }
        Ok(removed)
    }

    pub fn run_worker(&self) -> io::Result<()> {
        while !self.stopping.load(Ordering::Acquire) {
            self.promote_due_deferred()?;
            let wait = self.next_deferred_wait(Duration::from_secs(1))?;
            let first = match self.receiver.recv_timeout(wait) {
                Ok(Work::Source(source)) => source,
                Ok(Work::Stop) | Err(RecvTimeoutError::Disconnected) => return Ok(()),
                Err(RecvTimeoutError::Timeout) => continue,
            };
            if self.defer_if_unsettled(&first)? {
                continue;
            }
            let batch = self.gather_batch(first)?;
            self.process_batch(&batch)?;
        }
        Ok(())
    }

    fn gather_batch(&self, first: PathBuf) -> io::Result<Vec<PathBuf>> {
        let mut batch = vec![first];
        let deadline = Instant::now() + self.config.batch_delay;
        while batch.len() < self.config.batch_max_files {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.receiver.recv_timeout(remaining) {
                Ok(Work::Source(source)) => {
                    if self.defer_if_unsettled(&source)? {
                        continue;
                    }
                    batch.push(source);
                }
                Ok(Work::Stop) => {
                    let _ = self.sender.send(Work::Stop);
                    break;
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(batch)
    }

    fn process_batch(&self, batch: &[PathBuf]) -> io::Result<()> {
        let copy_started = Instant::now();
        let mut prepared = Vec::new();
        let mut status_counts = HashMap::<String, u64>::new();
        for source in batch {
            if self.defer_if_unsettled(source)? {
                bump(&mut status_counts, "settling");
                continue;
            }
            let start_seq = self.current_seq(source)?;
            let (status, item) = self.prepare_copy(source, start_seq);
            bump(&mut status_counts, &status.label());
            if let Some(item) = item {
                prepared.push(item);
            } else {
                self.finish_source(source, false, Some(start_seq))?;
            }
        }
        let copy_time = copy_started.elapsed();

        let sync_started = Instant::now();
        let sync_result = self.sync_prepared(&mut prepared);
        let sync_time = sync_started.elapsed();
        let sync_ok = sync_result.is_ok();
        let sync_error = sync_result
            .as_ref()
            .err()
            .map_or_else(|| "-".to_owned(), std::string::ToString::to_string);

        let copied: u64 = prepared.iter().map(|item| item.copied).sum();
        let mut clean = 0_u64;
        let mut retry_count = 0_u64;
        let mut state_errors = 0_u64;
        for item in prepared {
            let current_seq = self.current_seq(&item.source)?;
            let current_gen = self.overlay_generation(&item.source)?;
            let mut retry = !sync_ok
                || item.changed_during_copy
                || current_seq != item.start_seq
                || current_gen != Some(item.clean_gen);
            if !retry {
                let commit = (|| -> io::Result<()> {
                    self.store.write_generation(&item.source, item.clean_gen)?;
                    if self.journal.create_marker_exists(&item.source)? {
                        self.journal.clear_create_marker(&item.source)?;
                    }
                    if self.journal.rename_marker_exists(&item.source)? {
                        self.finalize_rename(&item.source)?;
                    }
                    Ok(())
                })();
                if commit.is_ok() {
                    clean = clean.saturating_add(1);
                } else {
                    state_errors = state_errors.saturating_add(1);
                    retry = true;
                }
            }
            if retry {
                retry_count = retry_count.saturating_add(1);
            }
            self.finish_source(&item.source, retry, Some(item.start_seq))?;
        }

        println!(
            "batch files={} prepared={} clean={} retry={} state_errors={} copy_bytes={} copy_ms={:.1} durability={} sync_ms={:.1} sync_ok={} sync_error={} status={status_counts:?}",
            batch.len(),
            status_counts.get("prepared").copied().unwrap_or(0),
            clean,
            retry_count,
            state_errors,
            copied,
            copy_time.as_secs_f64() * 1000.0,
            match self.config.durability {
                Durability::SyncFs => "syncfs",
                Durability::File => "file",
            },
            sync_time.as_secs_f64() * 1000.0,
            u8::from(sync_ok),
            sync_error,
        );
        Ok(())
    }

    fn sync_prepared(&self, prepared: &mut [Prepared]) -> io::Result<()> {
        if prepared.is_empty() {
            return Ok(());
        }
        match self.config.durability {
            Durability::SyncFs => syncfs(&self.sync_file).map_err(io::Error::from),
            Durability::File => {
                let mut dirs = BTreeSet::new();
                for item in prepared {
                    let file = item.canonical_file.as_ref().ok_or_else(|| {
                        io::Error::new(ErrorKind::InvalidData, "missing canonical file")
                    })?;
                    file.sync_all()?;
                    dirs.extend(item.sync_dirs.iter().cloned());
                }
                for directory in dirs {
                    sync_directory(&directory)?;
                }
                Ok(())
            }
        }
    }

    fn current_seq(&self, source: &Path) -> io::Result<u64> {
        Ok(self
            .lock_scheduler()?
            .seq
            .get(source)
            .copied()
            .unwrap_or_default())
    }

    pub fn finish_source(
        &self,
        source: &Path,
        retry: bool,
        expected_seq: Option<u64>,
    ) -> io::Result<()> {
        let requeue = {
            let mut state = self.lock_scheduler()?;
            let changed = expected_seq.is_some_and(|expected| {
                state.seq.get(source).copied().unwrap_or_default() != expected
            });
            let requeue = (retry || changed) && !self.stopping.load(Ordering::Acquire);
            if !requeue {
                state.pending.remove(source);
            }
            requeue
        };
        if requeue {
            self.sender
                .send(Work::Source(source.to_path_buf()))
                .map_err(channel_closed)?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
enum PrepareError {
    #[error("{0:?}")]
    Status(PrepareStatus),
    #[error(transparent)]
    Io(#[from] io::Error),
}

fn channel_closed<T>(_error: crossbeam_channel::SendError<T>) -> io::Error {
    io::Error::new(ErrorKind::BrokenPipe, "checkpoint work queue closed")
}

fn open_read_nofollow(path: &Path) -> io::Result<File> {
    open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(io::Error::from)
}

pub(crate) fn open_read_nofollow_for_admission(path: &Path) -> io::Result<File> {
    open_read_nofollow(path)
}

fn open_write_nofollow(path: &Path) -> io::Result<File> {
    open(
        path,
        OFlags::WRONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(io::Error::from)
}

fn create_write_nofollow(path: &Path, raw_mode: u32) -> io::Result<File> {
    open(
        path,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::from_raw_mode(raw_mode),
    )
    .map(File::from)
    .map_err(io::Error::from)
}

fn apply_owner_mode_best_effort(file: &File, metadata: &fs::Metadata) {
    let _ = fchmod(file, Mode::from_raw_mode(metadata.mode() & 0o7777));
    let _ = fchown(
        file,
        Some(Uid::from_raw(metadata.uid())),
        Some(Gid::from_raw(metadata.gid())),
    );
}

fn apply_metadata_best_effort(file: &File, metadata: &fs::Metadata) {
    apply_owner_mode_best_effort(file, metadata);
    let times = Timestamps {
        last_access: Timespec {
            tv_sec: metadata.atime(),
            tv_nsec: metadata.atime_nsec(),
        },
        last_modification: Timespec {
            tv_sec: metadata.mtime(),
            tv_nsec: metadata.mtime_nsec(),
        },
    };
    let _ = futimens(file, &times);
}

fn lexists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn bump(counts: &mut HashMap<String, u64>, key: &str) {
    let value = counts.entry(key.to_owned()).or_default();
    *value = value.saturating_add(1);
}

fn collect_suffix_files(root: &Path, suffix: &[u8]) -> io::Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                if let Some(io_error) = error.io_error()
                    && io_error.kind() != ErrorKind::NotFound
                {
                    return Err(io::Error::new(io_error.kind(), io_error.to_string()));
                }
                continue;
            }
        };
        if entry.file_type().is_file() && entry.file_name().as_bytes().ends_with(suffix) {
            result.push(entry.into_path());
        }
    }
    Ok(result)
}

fn source_from_marker(
    root: &Path,
    layout: &Layout,
    marker: &Path,
    suffix: &[u8],
) -> io::Result<PathBuf> {
    let relative = marker
        .strip_prefix(root)
        .map_err(|_| io::Error::new(ErrorKind::InvalidData, "marker outside rename root"))?;
    let name = relative
        .file_name()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "marker has no file name"))?;
    let base = name
        .as_bytes()
        .strip_suffix(suffix)
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "marker suffix mismatch"))?;
    let relative = relative.with_file_name(OsString::from_vec(base.to_vec()));
    layout.source_from_relative(&relative)
}

#[cfg(test)]
#[path = "checkpoint_tests.rs"]
mod tests;
