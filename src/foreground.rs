use std::collections::{BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    AccessFlags, BackingId, BsdFileFlags, CopyFileRangeFlags, Errno, FileAttr, FileHandle,
    FileType, Filesystem, FopenFlags, Generation as FuseGeneration, INodeNo, InitFlags,
    KernelConfig, LockOwner, OpenAccMode, OpenFlags, RenameFlags as FuseRenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use parking_lot::Mutex;
use rustix::fs::{self as rfs, AtFlags, Mode, OFlags, fchmod, fchown, futimens};
use rustix::fs::{Timespec, Timestamps};
use xattr::FileExt as XattrFileExt;

use crate::store::{remove_if_exists, sync_directory};
use crate::{
    Generation, Layout, MetadataStore, MicroRead, MicroStore, NamespaceJournal, Policy, RangeCache,
    RangeReadState, Tier,
};

const TTL: Duration = Duration::from_millis(250);
const DEFAULT_WRITEBACK_COPY_MAX: u64 = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct TierState {
    pub layout: Layout,
    pub metadata: Arc<dyn MetadataStore>,
    pub journal: Arc<dyn NamespaceJournal>,
    pub checkpoint_socket: PathBuf,
}

#[derive(Clone)]
pub struct ForegroundConfig {
    pub source_root: PathBuf,
    pub policy: Policy,
    pub durable: TierState,
    pub volatile: TierState,
    pub micro: Arc<dyn MicroStore>,
    pub range_cache: Option<Arc<RangeCache>>,
    pub admission_socket: PathBuf,
    pub defer_file_namespace: bool,
    pub relaxed_create_durability: bool,
    pub writeback_copy_max: u64,
}

impl ForegroundConfig {
    #[must_use]
    pub const fn with_default_copy_limit(mut self) -> Self {
        if self.writeback_copy_max == 0 {
            self.writeback_copy_max = DEFAULT_WRITEBACK_COPY_MAX;
        }
        self
    }
}

mod inode;
mod source;

use inode::InodeTable;
use source::SourceRoot;

enum HandleData {
    File(File),
    Bytes(Arc<[u8]>),
}

struct OpenHandle {
    logical: PathBuf,
    data: HandleData,
    writable: bool,
    overlay: bool,
    _backing: Option<Arc<BackingId>>,
    range: Option<Arc<Mutex<RangeReadState>>>,
}

impl OpenHandle {
    fn new(
        logical: PathBuf,
        data: HandleData,
        writable: bool,
        overlay: bool,
        backing: Option<Arc<BackingId>>,
        range: Option<RangeReadState>,
    ) -> Self {
        Self {
            logical,
            data,
            writable,
            overlay,
            _backing: backing,
            range: range.map(|state| Arc::new(Mutex::new(state))),
        }
    }
}

struct DirectoryHandle {
    entries: Arc<[(INodeNo, FileType, OsString)]>,
}

#[derive(Clone, Copy)]
struct RenameRequest<'a> {
    old: &'a Path,
    new: &'a Path,
    old_parent: &'a Path,
    old_name: &'a OsStr,
    new_parent: &'a Path,
    new_name: &'a OsStr,
    flags: FuseRenameFlags,
}

#[derive(Debug)]
struct VisibleMetadata {
    identity: (u64, u64),
    metadata: fs::Metadata,
    source_exists: bool,
    overlay_exists: bool,
}

#[derive(Clone, Copy)]
struct SetAttrUpdate {
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    size: Option<u64>,
    atime: Option<TimeOrNow>,
    mtime: Option<TimeOrNow>,
}

pub struct MoraineFs {
    source: SourceRoot,
    config: ForegroundConfig,
    inodes: Mutex<InodeTable>,
    handles: Mutex<HashMap<u64, OpenHandle>>,
    directories: Mutex<HashMap<u64, DirectoryHandle>>,
    next_handle: AtomicU64,
}

impl MoraineFs {
    pub fn new(mut config: ForegroundConfig) -> io::Result<Self> {
        let source = SourceRoot::new(config.source_root.clone())?;
        config.source_root.clone_from(&source.path);
        if config.durable.layout.source_root != source.path
            || config.volatile.layout.source_root != source.path
        {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "tier layouts must use the canonical source root",
            ));
        }
        for runtime in [&config.durable, &config.volatile] {
            fs::create_dir_all(&runtime.layout.overlay_root)?;
            fs::create_dir_all(runtime.layout.transaction_root())?;
        }
        let root_metadata = source.metadata(&source.path)?;
        let identity = (root_metadata.dev(), root_metadata.ino());
        config = config.with_default_copy_limit();
        let root_path = config.source_root.clone();
        Ok(Self {
            source,
            config,
            inodes: Mutex::new(InodeTable::new(root_path, identity)),
            handles: Mutex::new(HashMap::new()),
            directories: Mutex::new(HashMap::new()),
            next_handle: AtomicU64::new(1),
        })
    }

    fn runtime(&self, logical: &Path) -> &TierState {
        match self.config.policy.tier_for(logical) {
            Tier::Durable => &self.config.durable,
            Tier::Volatile => &self.config.volatile,
        }
    }

    fn inode_path(&self, ino: INodeNo) -> io::Result<PathBuf> {
        self.inodes
            .lock()
            .path(ino)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))
    }

    fn remember_lookup(&self, ino: INodeNo) {
        self.inodes.lock().remember(ino);
    }

    fn visible_metadata(&self, logical: &Path) -> io::Result<VisibleMetadata> {
        let runtime = self.runtime(logical);
        let source_metadata = match self.source.metadata(logical) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let overlay_path = runtime.layout.overlay_path(logical)?;
        let overlay_metadata = match fs::symlink_metadata(&overlay_path) {
            Ok(metadata) if metadata.is_file() => Some(metadata),
            Ok(_) => None,
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };

        match (source_metadata, overlay_metadata) {
            (Some(source), Some(overlay)) if source.is_file() => Ok(VisibleMetadata {
                identity: (source.dev(), source.ino()),
                metadata: overlay,
                source_exists: true,
                overlay_exists: true,
            }),
            (Some(source), _) => Ok(VisibleMetadata {
                identity: (source.dev(), source.ino()),
                metadata: source,
                source_exists: true,
                overlay_exists: false,
            }),
            (None, Some(overlay)) => {
                let created = runtime.journal.create_marker_exists(logical)?;
                let renamed = runtime.journal.rename_marker_exists(logical)?
                    && runtime.journal.rename_ready_exists(logical)?;
                if !created && !renamed {
                    return Err(io::Error::from_raw_os_error(libc::ENOENT));
                }
                Ok(VisibleMetadata {
                    identity: (overlay.dev(), overlay.ino()),
                    metadata: overlay,
                    source_exists: false,
                    overlay_exists: true,
                })
            }
            (None, None) => Err(io::Error::from_raw_os_error(libc::ENOENT)),
        }
    }

    fn attr_for_path(&self, logical: &Path) -> io::Result<FileAttr> {
        let visible = self.visible_metadata(logical)?;
        let ino = self
            .inodes
            .lock()
            .get_or_insert(logical.to_path_buf(), visible.identity);
        Ok(metadata_attr(&visible.metadata, ino))
    }

    fn child_path(&self, parent: INodeNo, name: &OsStr) -> io::Result<PathBuf> {
        let parent = self.inode_path(parent)?;
        SourceRoot::child(&parent, name)
    }

    fn overlay_is_clean(logical: &Path, runtime: &TierState) -> bool {
        let Ok(metadata) = fs::metadata(runtime.layout.overlay_path(logical).unwrap_or_default())
        else {
            return false;
        };
        let Ok(generation) = Generation::from_metadata(&metadata) else {
            return false;
        };
        runtime.metadata.read_generation(logical).ok().flatten() == Some(generation)
    }

    fn remove_overlay(logical: &Path, runtime: &TierState) -> io::Result<()> {
        remove_if_exists(&runtime.layout.overlay_path(logical)?)?;
        runtime.metadata.remove_generation(logical)
    }

    fn ensure_overlay_parent(logical: &Path, runtime: &TierState) -> io::Result<PathBuf> {
        let path = runtime.layout.overlay_path(logical)?;
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "overlay path has no parent"))?;
        fs::create_dir_all(parent)?;
        Ok(path)
    }

    fn materialize_overlay(
        &self,
        logical: &Path,
        runtime: &TierState,
        flags: OpenFlags,
    ) -> io::Result<Option<File>> {
        let overlay = Self::ensure_overlay_parent(logical, runtime)?;
        if overlay.is_file() {
            return open_overlay(&overlay, flags).map(Some);
        }

        let source_metadata = self.source.metadata(logical)?;
        if !source_metadata.is_file() {
            return Ok(None);
        }
        let truncating = flags.0 & libc::O_TRUNC != 0;
        if !truncating
            && self.config.policy.tier_for(logical) == Tier::Durable
            && source_metadata.len() > self.config.writeback_copy_max
        {
            return Ok(None);
        }

        let source = self
            .source
            .open_file(logical, OFlags::RDONLY, Mode::empty())?;
        let temp = overlay_temp_path(&overlay, self.next_handle.fetch_add(1, Ordering::Relaxed));
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .mode(source_metadata.mode() & 0o7777);
        let mut output = options.open(&temp)?;
        if !truncating {
            let mut input = source;
            io::copy(&mut input, &mut output)?;
        }
        apply_metadata(&output, &source_metadata)?;
        output.sync_data()?;
        drop(output);
        fs::rename(&temp, &overlay)?;
        sync_directory(
            overlay
                .parent()
                .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "overlay has no parent"))?,
        )?;
        runtime.metadata.remove_generation(logical)?;
        open_overlay(&overlay, flags).map(Some)
    }

    fn open_visible(&self, logical: &Path, flags: OpenFlags) -> io::Result<(HandleData, bool)> {
        let runtime = self.runtime(logical);
        let writable = flags.acc_mode() != OpenAccMode::O_RDONLY;
        if writable {
            let _ = self.config.micro.invalidate(logical);
            if let Some(file) = self.materialize_overlay(logical, runtime, flags)? {
                return Ok((HandleData::File(file), true));
            }
            return self
                .source
                .open_file(logical, open_flags(flags)?, Mode::empty())
                .map(|file| (HandleData::File(file), false));
        }

        let overlay = runtime.layout.overlay_path(logical)?;
        if overlay.is_file() {
            return open_overlay(&overlay, flags).map(|file| (HandleData::File(file), true));
        }
        let source = self
            .source
            .open_file(logical, OFlags::RDONLY, Mode::empty())?;
        let metadata = source.metadata()?;
        match self.config.micro.open_valid(logical, &metadata)? {
            Some(MicroRead::File(file)) => Ok((HandleData::File(file), false)),
            Some(MicroRead::Bytes(bytes)) => Ok((HandleData::Bytes(bytes), false)),
            None => {
                self.notify_admission(logical.parent().unwrap_or(&self.source.path));
                Ok((HandleData::File(source), false))
            }
        }
    }

    fn insert_file_handle(&self, ino: INodeNo, handle: OpenHandle) -> FileHandle {
        let raw = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.handles.lock().insert(raw, handle);
        self.inodes.lock().open(ino);
        FileHandle(raw)
    }

    fn notify_checkpoint(&self, logical: &Path) {
        let runtime = self.runtime(logical);
        send_datagram(&runtime.checkpoint_socket, logical.as_os_str().as_bytes());
    }

    fn notify_admission(&self, directory: &Path) {
        send_datagram(
            &self.config.admission_socket,
            directory.as_os_str().as_bytes(),
        );
    }

    fn directory_entries(&self, logical: &Path) -> io::Result<Vec<(INodeNo, FileType, OsString)>> {
        let mut names = self
            .source
            .list(logical)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        for runtime in [&self.config.durable, &self.config.volatile] {
            let overlay_dir = runtime.layout.overlay_path(logical)?;
            let Ok(entries) = fs::read_dir(overlay_dir) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let name = entry.file_name();
                let child = logical.join(&name);
                let visible = runtime
                    .journal
                    .create_marker_exists(&child)
                    .unwrap_or(false)
                    || (runtime
                        .journal
                        .rename_marker_exists(&child)
                        .unwrap_or(false)
                        && runtime.journal.rename_ready_exists(&child).unwrap_or(false));
                if visible {
                    names.insert(name);
                }
            }
        }

        let parent = if logical == self.source.path {
            logical
        } else {
            logical.parent().unwrap_or(&self.source.path)
        };
        let mut visible_entries = Vec::with_capacity(names.len().saturating_add(2));
        visible_entries.push((
            logical.to_path_buf(),
            self.visible_metadata(logical)?,
            OsString::from("."),
        ));
        visible_entries.push((
            parent.to_path_buf(),
            self.visible_metadata(parent)?,
            OsString::from(".."),
        ));
        for name in names {
            let child = logical.join(&name);
            let Ok(visible) = self.visible_metadata(&child) else {
                continue;
            };
            visible_entries.push((child, visible, name));
        }

        let mut inodes = self.inodes.lock();
        let mut result = Vec::with_capacity(visible_entries.len());
        for (path, visible, name) in visible_entries {
            let ino = inodes.get_or_insert(path, visible.identity);
            inodes.pin(ino);
            result.push((ino, metadata_kind(&visible.metadata), name));
        }
        drop(inodes);
        Ok(result)
    }

    fn apply_setattr(
        &self,
        ino: INodeNo,
        fh: Option<FileHandle>,
        update: SetAttrUpdate,
    ) -> io::Result<FileAttr> {
        let logical = self.inode_path(ino)?;
        let visible = self.visible_metadata(&logical)?;
        if visible.metadata.is_file() || visible.metadata.is_dir() {
            self.apply_open_setattr(ino, &logical, fh, visible.metadata.is_file(), update)
        } else {
            self.apply_special_setattr(&logical, &visible, update)
        }
    }

    fn apply_special_setattr(
        &self,
        logical: &Path,
        visible: &VisibleMetadata,
        update: SetAttrUpdate,
    ) -> io::Result<FileAttr> {
        if update.size.is_some() {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let parent = logical
            .parent()
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "special file has no parent"))?;
        let name = logical
            .file_name()
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "special file has no name"))?;
        let parent_fd = self.source.open_parent(parent)?;
        if update.uid.is_some() || update.gid.is_some() {
            rfs::chownat(
                &parent_fd,
                name,
                update.uid.map(rfs::Uid::from_raw),
                update.gid.map(rfs::Gid::from_raw),
                AtFlags::SYMLINK_NOFOLLOW,
            )
            .map_err(io::Error::from)?;
        }
        if let Some(mode) = update.mode {
            let flags = if visible.metadata.file_type().is_symlink() {
                AtFlags::SYMLINK_NOFOLLOW
            } else {
                AtFlags::empty()
            };
            rfs::chmodat(&parent_fd, name, Mode::from_raw_mode(mode & 0o7777), flags)
                .map_err(io::Error::from)?;
        }
        if update.atime.is_some() || update.mtime.is_some() {
            let current = self.source.metadata(logical)?;
            let times = Timestamps {
                last_access: time_or_existing(update.atime, current.atime(), current.atime_nsec()),
                last_modification: time_or_existing(
                    update.mtime,
                    current.mtime(),
                    current.mtime_nsec(),
                ),
            };
            rfs::utimensat(&parent_fd, name, &times, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(io::Error::from)?;
        }
        self.attr_for_path(logical)
    }

    fn open_setattr_target(
        &self,
        logical: &Path,
        fh: Option<FileHandle>,
        is_file: bool,
    ) -> io::Result<File> {
        if let Some(fh) = fh {
            let handles = self.handles.lock();
            return match handles.get(&fh.0).map(|handle| &handle.data) {
                Some(HandleData::File(file)) => file.try_clone(),
                Some(HandleData::Bytes(_)) => Err(io::Error::from_raw_os_error(libc::EROFS)),
                None => Err(io::Error::from_raw_os_error(libc::EBADF)),
            };
        }
        if is_file {
            let runtime = self.runtime(logical);
            if let Some(file) =
                self.materialize_overlay(logical, runtime, OpenFlags(libc::O_RDWR))?
            {
                return Ok(file);
            }
            return self.source.open_file(logical, OFlags::RDWR, Mode::empty());
        }
        self.source
            .open_file(logical, OFlags::RDONLY | OFlags::NOFOLLOW, Mode::empty())
    }

    fn apply_open_setattr(
        &self,
        ino: INodeNo,
        logical: &Path,
        fh: Option<FileHandle>,
        is_file: bool,
        update: SetAttrUpdate,
    ) -> io::Result<FileAttr> {
        let file = self.open_setattr_target(logical, fh, is_file)?;
        if update.uid.is_some() || update.gid.is_some() {
            fchown(
                &file,
                update.uid.map(rfs::Uid::from_raw),
                update.gid.map(rfs::Gid::from_raw),
            )
            .map_err(io::Error::from)?;
        }
        if let Some(mode) = update.mode {
            fchmod(&file, Mode::from_raw_mode(mode & 0o7777)).map_err(io::Error::from)?;
        }
        if let Some(size) = update.size {
            file.set_len(size)?;
        }
        if update.atime.is_some() || update.mtime.is_some() {
            let current = file.metadata()?;
            let times = Timestamps {
                last_access: time_or_existing(update.atime, current.atime(), current.atime_nsec()),
                last_modification: time_or_existing(
                    update.mtime,
                    current.mtime(),
                    current.mtime_nsec(),
                ),
            };
            futimens(&file, &times).map_err(io::Error::from)?;
        }
        let metadata = file.metadata()?;
        if metadata.is_file() {
            let _ = self.config.micro.invalidate(logical);
            let runtime = self.runtime(logical);
            if runtime.layout.overlay_path(logical)?.is_file() {
                runtime.metadata.remove_generation(logical)?;
                self.notify_checkpoint(logical);
            }
        }
        Ok(metadata_attr(&metadata, ino))
    }

    fn creation_identity(&self, parent: &Path, req: &Request) -> io::Result<(u32, u32, bool)> {
        let metadata = self.source.metadata(parent)?;
        let inherit_setgid = metadata.mode() & libc::S_ISGID != 0;
        let gid = if inherit_setgid {
            metadata.gid()
        } else {
            req.gid()
        };
        Ok((req.uid(), gid, inherit_setgid))
    }

    fn dirty_rename(&self, request: RenameRequest<'_>) -> io::Result<()> {
        let RenameRequest {
            old,
            new,
            old_parent,
            old_name,
            new_parent,
            new_name,
            flags,
        } = request;
        if !flags.is_empty() {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let old_runtime = self.runtime(old);
        let new_runtime = self.runtime(new);
        if !Arc::ptr_eq(&old_runtime.metadata, &new_runtime.metadata) {
            return Err(io::Error::from_raw_os_error(libc::EXDEV));
        }
        if old_runtime.journal.rename_marker_exists(old)?
            || old_runtime.journal.rename_marker_exists(new)?
        {
            return Err(io::Error::from_raw_os_error(libc::EBUSY));
        }
        let old_overlay = old_runtime.layout.overlay_path(old)?;
        let new_overlay = old_runtime.layout.overlay_path(new)?;
        if !old_overlay.is_file() {
            self.source
                .rename(old_parent, old_name, new_parent, new_name, flags)?;
            self.inodes.lock().rename_prefix(old, new);
            let _ = self.config.micro.invalidate(old);
            let _ = self.config.micro.invalidate(new);
            return Ok(());
        }
        if Self::overlay_is_clean(old, old_runtime) {
            Self::remove_overlay(old, old_runtime)?;
            self.source
                .rename(old_parent, old_name, new_parent, new_name, flags)?;
            self.inodes.lock().rename_prefix(old, new);
            return Ok(());
        }

        let old_canonical = self.source.metadata(old).ok();
        let new_canonical = self.source.metadata(new).ok();
        if old_canonical.is_none() && !old_runtime.journal.create_marker_exists(old)? {
            return Err(io::Error::from_raw_os_error(libc::ENOENT));
        }
        if old_canonical.is_none() && new_canonical.is_some() {
            return Err(io::Error::from_raw_os_error(libc::EBUSY));
        }

        fs::create_dir_all(new_overlay.parent().ok_or_else(|| {
            io::Error::new(ErrorKind::InvalidInput, "destination overlay has no parent")
        })?)?;
        old_runtime.journal.begin_rename(old, new)?;
        let backup = old_runtime.layout.rename_backup_path(new)?;
        let new_dirty = new_overlay.is_file() && !Self::overlay_is_clean(new, old_runtime);
        let mut backup_moved = false;
        if new_dirty {
            if backup.exists() {
                old_runtime.journal.clear_rename_marker(new)?;
                return Err(io::Error::from_raw_os_error(libc::EBUSY));
            }
            if let Some(parent) = backup.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(&new_overlay, &backup)?;
            backup_moved = true;
        } else if new_overlay.is_file() {
            Self::remove_overlay(new, old_runtime)?;
        }

        if let Err(error) = fs::rename(&old_overlay, &new_overlay) {
            if backup_moved {
                let _ = fs::rename(&backup, &new_overlay);
            }
            let _ = old_runtime.journal.clear_rename_marker(new);
            return Err(error);
        }
        if old_canonical.is_some()
            && let Err(error) = self
                .source
                .rename(old_parent, old_name, new_parent, new_name, flags)
        {
            let _ = fs::rename(&new_overlay, &old_overlay);
            if backup_moved {
                let _ = fs::rename(&backup, &new_overlay);
            }
            let _ = old_runtime.journal.clear_rename_marker(new);
            return Err(error);
        }

        old_runtime.metadata.remove_generation(old)?;
        old_runtime.metadata.remove_generation(new)?;
        if backup_moved {
            remove_if_exists(&backup)?;
        }
        old_runtime.journal.mark_rename_ready(new)?;
        self.inodes.lock().rename_prefix(old, new);
        let _ = self.config.micro.invalidate(old);
        let _ = self.config.micro.invalidate(new);
        self.notify_checkpoint(new);
        Ok(())
    }
}

fn open_flags(flags: OpenFlags) -> io::Result<OFlags> {
    let bits = u32::try_from(flags.0)
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "invalid open flags"))?;
    let mut out = OFlags::from_bits_retain(bits);
    out.remove(OFlags::CREATE | OFlags::EXCL);
    Ok(out | OFlags::CLOEXEC | OFlags::NOFOLLOW)
}

fn open_overlay(path: &Path, flags: OpenFlags) -> io::Result<File> {
    rfs::open(path, open_flags(flags)?, Mode::empty())
        .map(File::from)
        .map_err(io::Error::from)
}

fn overlay_temp_path(path: &Path, serial: u64) -> PathBuf {
    let name = path.file_name().unwrap_or_else(|| OsStr::new("overlay"));
    let mut raw = Vec::with_capacity(name.as_bytes().len() + 48);
    raw.push(b'.');
    raw.extend_from_slice(name.as_bytes());
    raw.extend_from_slice(format!(".morainefs.tmp.{}.{serial}", std::process::id()).as_bytes());
    path.with_file_name(OsString::from_vec(raw))
}

fn apply_created_file_identity(file: &File, uid: u32, gid: u32, mode: Mode) -> io::Result<()> {
    fchown(
        file,
        Some(rfs::Uid::from_raw(uid)),
        Some(rfs::Gid::from_raw(gid)),
    )
    .map_err(io::Error::from)?;
    fchmod(file, mode).map_err(io::Error::from)
}

fn apply_metadata(file: &File, metadata: &fs::Metadata) -> io::Result<()> {
    let _ = fchown(
        file,
        Some(rfs::Uid::from_raw(metadata.uid())),
        Some(rfs::Gid::from_raw(metadata.gid())),
    );
    fchmod(file, Mode::from_raw_mode(metadata.mode() & 0o7777)).map_err(io::Error::from)?;
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
    futimens(file, &times).map_err(io::Error::from)
}

fn metadata_attr(metadata: &fs::Metadata, ino: INodeNo) -> FileAttr {
    FileAttr {
        ino,
        size: metadata.len(),
        blocks: metadata.blocks(),
        atime: system_time(metadata.atime(), metadata.atime_nsec()),
        mtime: system_time(metadata.mtime(), metadata.mtime_nsec()),
        ctime: system_time(metadata.ctime(), metadata.ctime_nsec()),
        crtime: UNIX_EPOCH,
        kind: metadata_kind(metadata),
        perm: u16::try_from(metadata.mode() & 0o7777).unwrap_or(u16::MAX),
        nlink: u32::try_from(metadata.nlink()).unwrap_or(u32::MAX),
        uid: metadata.uid(),
        gid: metadata.gid(),
        rdev: u32::try_from(metadata.rdev()).unwrap_or(u32::MAX),
        flags: 0,
        blksize: u32::try_from(metadata.blksize()).unwrap_or(u32::MAX),
    }
}

fn metadata_kind(metadata: &fs::Metadata) -> FileType {
    let mode = metadata.mode() & libc::S_IFMT;
    match mode {
        libc::S_IFDIR => FileType::Directory,
        libc::S_IFLNK => FileType::Symlink,
        libc::S_IFBLK => FileType::BlockDevice,
        libc::S_IFCHR => FileType::CharDevice,
        libc::S_IFIFO => FileType::NamedPipe,
        libc::S_IFSOCK => FileType::Socket,
        _ => FileType::RegularFile,
    }
}

fn system_time(seconds: i64, nanos: i64) -> SystemTime {
    let nanos = u32::try_from(nanos).unwrap_or_default().min(999_999_999);
    if seconds >= 0 {
        UNIX_EPOCH
            .checked_add(Duration::new(
                u64::try_from(seconds).unwrap_or_default(),
                nanos,
            ))
            .unwrap_or(UNIX_EPOCH)
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::new(seconds.unsigned_abs(), nanos))
            .unwrap_or(UNIX_EPOCH)
    }
}

fn time_or_existing(value: Option<TimeOrNow>, seconds: i64, nanos: i64) -> Timespec {
    match value {
        Some(TimeOrNow::Now) => system_time_to_timespec(SystemTime::now()),
        Some(TimeOrNow::SpecificTime(time)) => system_time_to_timespec(time),
        None => Timespec {
            tv_sec: seconds,
            tv_nsec: nanos,
        },
    }
}

fn system_time_to_timespec(time: SystemTime) -> Timespec {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => Timespec {
            tv_sec: i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(duration.subsec_nanos()),
        },
        Err(error) => {
            let duration = error.duration();
            Timespec {
                tv_sec: -i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
                tv_nsec: i64::from(duration.subsec_nanos()),
            }
        }
    }
}

fn errno(error: &io::Error) -> Errno {
    Errno::from_i32(error.raw_os_error().unwrap_or(libc::EIO))
}

fn send_datagram(socket_path: &Path, payload: &[u8]) {
    let Ok(socket) = std::os::unix::net::UnixDatagram::unbound() else {
        return;
    };
    let _ = socket.send_to(payload, socket_path);
}

impl Filesystem for MoraineFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        config
            .add_capabilities(InitFlags::FUSE_PASSTHROUGH | InitFlags::FUSE_PARALLEL_DIROPS)
            .map_err(|unsupported| {
                io::Error::new(
                    ErrorKind::Unsupported,
                    format!("kernel lacks required FUSE capabilities: {unsupported:?}"),
                )
            })?;
        config.set_max_stack_depth(2).map_err(|maximum| {
            io::Error::new(
                ErrorKind::Unsupported,
                format!("kernel limits FUSE backing stack depth to {maximum}"),
            )
        })?;
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let result = self
            .child_path(parent, name)
            .and_then(|path| self.attr_for_path(&path));
        match result {
            Ok(attr) => {
                self.remember_lookup(attr.ino);
                reply.entry(&TTL, &attr, FuseGeneration(0));
            }
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.inodes.lock().forget(ino, nlookup);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        if let Some(fh) = fh {
            let metadata = {
                let handles = self.handles.lock();
                handles.get(&fh.0).and_then(|handle| match &handle.data {
                    HandleData::File(file) => file.metadata().ok(),
                    HandleData::Bytes(_) => None,
                })
            };
            if let Some(metadata) = metadata {
                reply.attr(&TTL, &metadata_attr(&metadata, ino));
                return;
            }
        }
        let result = self
            .inode_path(ino)
            .and_then(|path| self.attr_for_path(&path));
        match result {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let update = SetAttrUpdate {
            mode,
            uid,
            gid,
            size,
            atime,
            mtime,
        };
        match self.apply_setattr(ino, fh, update) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let result = self
            .inode_path(ino)
            .and_then(|logical| self.source.readlink(&logical));
        match result {
            Ok(target) => reply.data(&target),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let result = (|| -> io::Result<FileAttr> {
            let parent_path = self.inode_path(parent)?;
            let logical = SourceRoot::child(&parent_path, name)?;
            let (uid, gid, inherit_setgid) = self.creation_identity(&parent_path, req)?;
            let mut mode_bits = mode & 0o7777;
            if inherit_setgid {
                mode_bits |= libc::S_ISGID;
            }
            let create_mode = Mode::from_raw_mode(mode_bits);
            self.source.mkdir(&parent_path, name, create_mode)?;
            let directory = match self.source.open_raw(
                &logical,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
                Mode::empty(),
            ) {
                Ok(directory) => directory,
                Err(error) => {
                    let _ = self.source.unlink(&parent_path, name, true);
                    return Err(error);
                }
            };
            if let Err(error) = apply_created_file_identity(&directory, uid, gid, create_mode) {
                let _ = self.source.unlink(&parent_path, name, true);
                return Err(error);
            }
            self.attr_for_path(&logical)
        })();
        match result {
            Ok(attr) => {
                self.remember_lookup(attr.ino);
                reply.entry(&TTL, &attr, FuseGeneration(0));
            }
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let result = (|| -> io::Result<()> {
            let parent_path = self.inode_path(parent)?;
            let logical = SourceRoot::child(&parent_path, name)?;
            let runtime = self.runtime(&logical);
            let visible = self.visible_metadata(&logical)?;
            if visible.metadata.nlink() > 1 && visible.overlay_exists {
                if !Self::overlay_is_clean(&logical, runtime) {
                    return Err(io::Error::from_raw_os_error(libc::EBUSY));
                }
                Self::remove_overlay(&logical, runtime)?;
            }
            if runtime.journal.rename_marker_exists(&logical)? {
                if !runtime.journal.rename_ready_exists(&logical)? {
                    return Err(io::Error::from_raw_os_error(libc::EBUSY));
                }
                if let Some(old) = runtime.journal.read_rename_source(&logical)?
                    && runtime.journal.create_marker_exists(&old)?
                {
                    runtime.journal.clear_create_marker(&old)?;
                }
                runtime.journal.clear_rename_ready(&logical)?;
                runtime.journal.clear_rename_marker(&logical)?;
                remove_if_exists(&runtime.layout.rename_backup_path(&logical)?)?;
            }
            if runtime.journal.create_marker_exists(&logical)? {
                runtime.journal.clear_create_marker(&logical)?;
            }
            if visible.source_exists {
                self.source.unlink(&parent_path, name, false)?;
            }
            if visible.overlay_exists {
                Self::remove_overlay(&logical, runtime)?;
            }
            let _ = self.config.micro.invalidate(&logical);
            self.inodes.lock().remove_path(&logical);
            Ok(())
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let result = (|| -> io::Result<()> {
            let parent_path = self.inode_path(parent)?;
            let logical = SourceRoot::child(&parent_path, name)?;
            for runtime in [&self.config.durable, &self.config.volatile] {
                let overlay = runtime.layout.overlay_path(&logical)?;
                if overlay.is_dir() && fs::read_dir(&overlay)?.next().is_some() {
                    return Err(io::Error::from_raw_os_error(libc::ENOTEMPTY));
                }
            }
            self.source.unlink(&parent_path, name, true)?;
            for runtime in [&self.config.durable, &self.config.volatile] {
                let overlay = runtime.layout.overlay_path(&logical)?;
                match fs::remove_dir(overlay) {
                    Ok(()) => {}
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            self.inodes.lock().remove_path(&logical);
            Ok(())
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let result = (|| -> io::Result<FileAttr> {
            let parent_path = self.inode_path(parent)?;
            let logical = SourceRoot::child(&parent_path, link_name)?;
            let (uid, gid, _) = self.creation_identity(&parent_path, req)?;
            self.source.symlink(&parent_path, link_name, target)?;
            let parent_fd = self.source.open_parent(&parent_path)?;
            if let Err(error) = rfs::chownat(
                &parent_fd,
                link_name,
                Some(rfs::Uid::from_raw(uid)),
                Some(rfs::Gid::from_raw(gid)),
                AtFlags::SYMLINK_NOFOLLOW,
            )
            .map_err(io::Error::from)
            {
                let _ = self.source.unlink(&parent_path, link_name, false);
                return Err(error);
            }
            self.attr_for_path(&logical)
        })();
        match result {
            Ok(attr) => {
                self.remember_lookup(attr.ino);
                reply.entry(&TTL, &attr, FuseGeneration(0));
            }
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: FuseRenameFlags,
        reply: ReplyEmpty,
    ) {
        let result = (|| -> io::Result<()> {
            let old_parent = self.inode_path(parent)?;
            let destination_parent = self.inode_path(newparent)?;
            let old = SourceRoot::child(&old_parent, name)?;
            let new = SourceRoot::child(&destination_parent, newname)?;
            if self.config.policy.tier_for(&old) != self.config.policy.tier_for(&new) {
                return Err(io::Error::from_raw_os_error(libc::EXDEV));
            }
            self.dirty_rename(RenameRequest {
                old: &old,
                new: &new,
                old_parent: &old_parent,
                old_name: name,
                new_parent: &destination_parent,
                new_name: newname,
                flags,
            })
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let result = (|| -> io::Result<FileAttr> {
            let source = self.inode_path(ino)?;
            let destination_parent = self.inode_path(newparent)?;
            let new = SourceRoot::child(&destination_parent, newname)?;
            if self.config.policy.tier_for(&source) != self.config.policy.tier_for(&new) {
                return Err(io::Error::from_raw_os_error(libc::EXDEV));
            }
            let runtime = self.runtime(&source);
            if runtime.layout.overlay_path(&source)?.is_file()
                && !Self::overlay_is_clean(&source, runtime)
            {
                return Err(io::Error::from_raw_os_error(libc::EBUSY));
            }
            if runtime.layout.overlay_path(&source)?.is_file() {
                Self::remove_overlay(&source, runtime)?;
            }
            self.source
                .hard_link(&source, &destination_parent, newname)?;
            self.inodes.lock().alias(ino, new.clone());
            self.attr_for_path(&new)
        })();
        match result {
            Ok(attr) => {
                self.remember_lookup(attr.ino);
                reply.entry(&TTL, &attr, FuseGeneration(0));
            }
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let prepared = (|| -> io::Result<(PathBuf, HandleData, bool, bool)> {
            let logical = self.inode_path(ino)?;
            let writable = flags.acc_mode() != OpenAccMode::O_RDONLY;
            let (data, overlay) = self.open_visible(&logical, flags)?;
            Ok((logical, data, writable, overlay))
        })();
        let (logical, data, writable, overlay) = match prepared {
            Ok(value) => value,
            Err(error) => {
                reply.error(errno(&error));
                return;
            }
        };
        match data {
            HandleData::File(file) => {
                let range = if !writable
                    && !overlay
                    && self.config.range_cache.is_some()
                    && file.metadata().is_ok_and(|metadata| {
                        metadata.is_file() && metadata.len() > 2 * 1024 * 1024
                    }) {
                    Some(RangeReadState::default())
                } else {
                    None
                };
                if range.is_some() {
                    let fh = self.insert_file_handle(
                        ino,
                        OpenHandle::new(logical, HandleData::File(file), false, false, None, range),
                    );
                    reply.opened(fh, FopenFlags::FOPEN_DIRECT_IO);
                    return;
                }
                match reply.open_backing(&file) {
                    Ok(backing) => {
                        let backing = Arc::new(backing);
                        let fh = self.insert_file_handle(
                            ino,
                            OpenHandle::new(
                                logical,
                                HandleData::File(file),
                                writable,
                                overlay,
                                Some(Arc::clone(&backing)),
                                None,
                            ),
                        );
                        reply.opened_passthrough(fh, FopenFlags::empty(), &backing);
                    }
                    Err(error) => reply.error(errno(&error)),
                }
            }
            HandleData::Bytes(bytes) => {
                let fh = self.insert_file_handle(
                    ino,
                    OpenHandle::new(logical, HandleData::Bytes(bytes), false, false, None, None),
                );
                reply.opened(fh, FopenFlags::FOPEN_DIRECT_IO);
            }
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        enum ReadSource {
            File(File, Option<Arc<Mutex<RangeReadState>>>),
            Bytes(Arc<[u8]>),
        }

        let result = (|| -> io::Result<Vec<u8>> {
            let source = {
                let handles = self.handles.lock();
                let handle = handles
                    .get(&fh.0)
                    .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
                let source = match &handle.data {
                    HandleData::File(file) => {
                        ReadSource::File(file.try_clone()?, handle.range.clone())
                    }
                    HandleData::Bytes(bytes) => ReadSource::Bytes(Arc::clone(bytes)),
                };
                drop(handles);
                source
            };
            let requested = usize::try_from(size)
                .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "read size overflow"))?;
            match source {
                ReadSource::File(file, range) => {
                    if let (Some(cache), Some(range)) = (&self.config.range_cache, range) {
                        let mut state = range.lock();
                        if let Some(data) = cache.read(&file, offset, requested, &mut state)? {
                            return Ok(data);
                        }
                    }
                    let mut buffer = vec![0_u8; requested];
                    let read = file.read_at(&mut buffer, offset)?;
                    buffer.truncate(read);
                    Ok(buffer)
                }
                ReadSource::Bytes(bytes) => {
                    let start = usize::try_from(offset)
                        .unwrap_or(usize::MAX)
                        .min(bytes.len());
                    let end = start.saturating_add(requested).min(bytes.len());
                    Ok(bytes[start..end].to_vec())
                }
            }
        })();
        match result {
            Ok(data) => reply.data(&data),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let result = (|| -> io::Result<u32> {
            let file = {
                let handles = self.handles.lock();
                let handle = handles
                    .get(&fh.0)
                    .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
                let HandleData::File(file) = &handle.data else {
                    return Err(io::Error::from_raw_os_error(libc::EBADF));
                };
                let file = file.try_clone()?;
                drop(handles);
                file
            };
            let written = file.write_at(data, offset)?;
            u32::try_from(written)
                .map_err(|_| io::Error::new(ErrorKind::InvalidData, "write size overflow"))
        })();
        match result {
            Ok(written) => reply.written(written),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        let handles = self.handles.lock();
        if handles.contains_key(&fh.0) {
            reply.ok();
        } else {
            reply.error(Errno::EBADF);
        }
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let handle = self.handles.lock().remove(&fh.0);
        let Some(handle) = handle else {
            reply.error(Errno::EBADF);
            return;
        };
        if handle.writable && handle.overlay {
            self.notify_checkpoint(&handle.logical);
        }
        self.inodes.lock().close(ino);
        reply.ok();
        drop(handle);
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        let result = (|| -> io::Result<()> {
            let (file, notify_path) = {
                let handles = self.handles.lock();
                let handle = handles
                    .get(&fh.0)
                    .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
                let HandleData::File(file) = &handle.data else {
                    return Ok(());
                };
                let notify_path =
                    (handle.writable && handle.overlay).then(|| handle.logical.clone());
                let file = file.try_clone()?;
                drop(handles);
                (file, notify_path)
            };
            if datasync {
                file.sync_data()?;
            } else {
                file.sync_all()?;
            }
            if let Some(logical) = notify_path {
                self.notify_checkpoint(&logical);
            }
            Ok(())
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn mknod(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        let result = (|| -> io::Result<FileAttr> {
            let parent_path = self.inode_path(parent)?;
            let child = SourceRoot::child(&parent_path, name)?;
            let (uid, gid, _) = self.creation_identity(&parent_path, req)?;
            let parent_fd = self.source.open_parent(&parent_path)?;
            let kind = rfs::FileType::from_raw_mode(mode);
            let create_mode = Mode::from_raw_mode(mode & 0o7777);
            rfs::mknodat(&parent_fd, name, kind, create_mode, u64::from(rdev))
                .map_err(io::Error::from)?;
            let identity_result = (|| -> io::Result<()> {
                rfs::chownat(
                    &parent_fd,
                    name,
                    Some(rfs::Uid::from_raw(uid)),
                    Some(rfs::Gid::from_raw(gid)),
                    AtFlags::SYMLINK_NOFOLLOW,
                )
                .map_err(io::Error::from)?;
                rfs::chmodat(&parent_fd, name, create_mode, AtFlags::empty())
                    .map_err(io::Error::from)
            })();
            if let Err(error) = identity_result {
                let _ = self.source.unlink(&parent_path, name, false);
                return Err(error);
            }
            self.attr_for_path(&child)
        })();
        match result {
            Ok(attr) => {
                self.remember_lookup(attr.ino);
                reply.entry(&TTL, &attr, FuseGeneration(0));
            }
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let result = (|| -> io::Result<FileHandle> {
            let logical = self.inode_path(ino)?;
            let entries = self.directory_entries(&logical)?;
            self.inodes.lock().open(ino);
            let raw = self.next_handle.fetch_add(1, Ordering::Relaxed);
            self.directories.lock().insert(
                raw,
                DirectoryHandle {
                    entries: entries.into(),
                },
            );
            Ok(FileHandle(raw))
        })();
        match result {
            Ok(fh) => reply.opened(fh, FopenFlags::FOPEN_CACHE_DIR),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let entries = self
            .directories
            .lock()
            .get(&fh.0)
            .map(|directory| Arc::clone(&directory.entries));
        let Some(entries) = entries else {
            reply.error(Errno::EBADF);
            return;
        };
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        for (index, (ino, kind, name)) in entries.iter().enumerate().skip(start) {
            let next = u64::try_from(index.saturating_add(1)).unwrap_or(u64::MAX);
            if reply.add(*ino, next, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let entries = self
            .directories
            .lock()
            .get(&fh.0)
            .map(|directory| Arc::clone(&directory.entries));
        let Some(entries) = entries else {
            reply.error(Errno::EBADF);
            return;
        };
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        for (index, (ino, _kind, name)) in entries.iter().enumerate().skip(start) {
            let Some(path) = self.inodes.lock().path(*ino) else {
                continue;
            };
            let Ok(attr) = self.attr_for_path(&path) else {
                continue;
            };
            let next = u64::try_from(index.saturating_add(1)).unwrap_or(u64::MAX);
            if reply.add(*ino, next, name, &TTL, &attr, FuseGeneration(0)) {
                break;
            }
            self.remember_lookup(*ino);
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        let directory = self.directories.lock().remove(&fh.0);
        let Some(directory) = directory else {
            reply.error(Errno::EBADF);
            return;
        };
        {
            let mut inodes = self.inodes.lock();
            for (entry_ino, _, _) in directory.entries.iter() {
                inodes.unpin(*entry_ino);
            }
            inodes.close(ino);
        }
        reply.ok();
    }

    fn fsyncdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        let result = self.inode_path(ino).and_then(|logical| {
            let directory = self.source.open_raw(
                &logical,
                OFlags::RDONLY | OFlags::DIRECTORY,
                Mode::empty(),
            )?;
            if datasync {
                directory.sync_data()
            } else {
                directory.sync_all()
            }
        });
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        match rfs::fstatvfs(&self.source.fd) {
            Ok(stat) => reply.statfs(
                stat.f_blocks,
                stat.f_bfree,
                stat.f_bavail,
                stat.f_files,
                stat.f_ffree,
                u32::try_from(stat.f_bsize).unwrap_or(u32::MAX),
                u32::try_from(stat.f_namemax).unwrap_or(u32::MAX),
                u32::try_from(stat.f_frsize).unwrap_or(u32::MAX),
            ),
            Err(error) => reply.error(errno(&io::Error::from(error))),
        }
    }

    fn setxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        let result = (|| -> io::Result<()> {
            let logical = self.inode_path(ino)?;
            let runtime = self.runtime(&logical);
            let overlay = runtime.layout.overlay_path(&logical)?;
            let xattr_flags =
                rfs::XattrFlags::from_bits_retain(u32::try_from(flags).unwrap_or_default());
            if overlay.is_file() {
                let file = rfs::open(
                    &overlay,
                    OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map(File::from)
                .map_err(io::Error::from)?;
                rfs::fsetxattr(&file, name, value, xattr_flags).map_err(io::Error::from)?;
                runtime.metadata.remove_generation(&logical)?;
                self.notify_checkpoint(&logical);
            } else if logical == self.source.path {
                let directory = self.source.open_raw(
                    &logical,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
                    Mode::empty(),
                )?;
                rfs::fsetxattr(&directory, name, value, xattr_flags).map_err(io::Error::from)?;
            } else {
                let anchored = self.source.anchored_path(&logical)?;
                rfs::lsetxattr(&anchored.path, name, value, xattr_flags)
                    .map_err(io::Error::from)?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let result = (|| -> io::Result<Vec<u8>> {
            let logical = self.inode_path(ino)?;
            let runtime = self.runtime(&logical);
            let overlay = runtime.layout.overlay_path(&logical)?;
            if overlay.is_file() {
                let file = rfs::open(
                    &overlay,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map(File::from)
                .map_err(io::Error::from)?;
                file.get_xattr(name)?
                    .ok_or_else(|| io::Error::from_raw_os_error(libc::ENODATA))
            } else if logical == self.source.path {
                let directory = self.source.open_raw(
                    &logical,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
                    Mode::empty(),
                )?;
                directory
                    .get_xattr(name)?
                    .ok_or_else(|| io::Error::from_raw_os_error(libc::ENODATA))
            } else {
                let anchored = self.source.anchored_path(&logical)?;
                xattr::get(&anchored.path, name)?
                    .ok_or_else(|| io::Error::from_raw_os_error(libc::ENODATA))
            }
        })();
        match result {
            Ok(value) if size == 0 => reply.size(u32::try_from(value.len()).unwrap_or(u32::MAX)),
            Ok(value) if usize::try_from(size).unwrap_or(usize::MAX) >= value.len() => {
                reply.data(&value);
            }
            Ok(_) => reply.error(Errno::ERANGE),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let result = (|| -> io::Result<Vec<u8>> {
            let logical = self.inode_path(ino)?;
            let runtime = self.runtime(&logical);
            let overlay = runtime.layout.overlay_path(&logical)?;
            let mut raw = Vec::new();
            if overlay.is_file() {
                let file = rfs::open(
                    &overlay,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map(File::from)
                .map_err(io::Error::from)?;
                for name in file.list_xattr()? {
                    raw.extend_from_slice(name.as_bytes());
                    raw.push(0);
                }
            } else if logical == self.source.path {
                let directory = self.source.open_raw(
                    &logical,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
                    Mode::empty(),
                )?;
                for name in directory.list_xattr()? {
                    raw.extend_from_slice(name.as_bytes());
                    raw.push(0);
                }
            } else {
                let anchored = self.source.anchored_path(&logical)?;
                for name in xattr::list(&anchored.path)? {
                    raw.extend_from_slice(name.as_bytes());
                    raw.push(0);
                }
            }
            Ok(raw)
        })();
        match result {
            Ok(value) if size == 0 => reply.size(u32::try_from(value.len()).unwrap_or(u32::MAX)),
            Ok(value) if usize::try_from(size).unwrap_or(usize::MAX) >= value.len() => {
                reply.data(&value);
            }
            Ok(_) => reply.error(Errno::ERANGE),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let result = (|| -> io::Result<()> {
            let logical = self.inode_path(ino)?;
            let runtime = self.runtime(&logical);
            let overlay = runtime.layout.overlay_path(&logical)?;
            if overlay.is_file() {
                let file = rfs::open(
                    &overlay,
                    OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map(File::from)
                .map_err(io::Error::from)?;
                rfs::fremovexattr(&file, name).map_err(io::Error::from)?;
                runtime.metadata.remove_generation(&logical)?;
                self.notify_checkpoint(&logical);
            } else if logical == self.source.path {
                let directory = self.source.open_raw(
                    &logical,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
                    Mode::empty(),
                )?;
                rfs::fremovexattr(&directory, name).map_err(io::Error::from)?;
            } else {
                let anchored = self.source.anchored_path(&logical)?;
                xattr::remove(&anchored.path, name)?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn access(&self, _req: &Request, ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        match self
            .inode_path(ino)
            .and_then(|logical| self.visible_metadata(&logical).map(|_| ()))
        {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let result = (|| -> io::Result<(FileAttr, FileHandle, Arc<BackingId>)> {
            let parent_path = self.inode_path(parent)?;
            let logical = SourceRoot::child(&parent_path, name)?;
            if flags & libc::O_EXCL != 0 && self.visible_metadata(&logical).is_ok() {
                return Err(io::Error::from_raw_os_error(libc::EEXIST));
            }
            let runtime = self.runtime(&logical);
            let deferred = self.config.defer_file_namespace
                || self.config.policy.tier_for(&logical) == Tier::Volatile;
            let raw_flags = OpenFlags(flags);
            let open = open_flags(raw_flags)? | OFlags::CREATE;
            let (uid, gid, _) = self.creation_identity(&parent_path, req)?;
            let file_mode = Mode::from_raw_mode(mode & 0o7777);

            let (file, overlay) = if deferred {
                let overlay_path = Self::ensure_overlay_parent(&logical, runtime)?;
                let mut overlay_flags = open;
                if flags & libc::O_EXCL != 0 {
                    overlay_flags |= OFlags::EXCL;
                }
                let file = rfs::open(&overlay_path, overlay_flags, file_mode)
                    .map(File::from)
                    .map_err(io::Error::from)?;
                let prepared = (|| -> io::Result<()> {
                    apply_created_file_identity(&file, uid, gid, file_mode)?;
                    if !self.config.relaxed_create_durability {
                        file.sync_data()?;
                        sync_directory(overlay_path.parent().ok_or_else(|| {
                            io::Error::new(ErrorKind::InvalidInput, "overlay has no parent")
                        })?)?;
                    }
                    runtime.journal.mark_created(&logical)
                })();
                if let Err(error) = prepared {
                    drop(file);
                    let _ = fs::remove_file(&overlay_path);
                    return Err(error);
                }
                (file, true)
            } else {
                let canonical = self.source.create_file(
                    &parent_path,
                    name,
                    open_flags(raw_flags)?,
                    file_mode,
                )?;
                if let Err(error) = apply_created_file_identity(&canonical, uid, gid, file_mode) {
                    let _ = self.source.unlink(&parent_path, name, false);
                    return Err(error);
                }
                let overlay_path = Self::ensure_overlay_parent(&logical, runtime)?;
                let overlay_result = (|| -> io::Result<File> {
                    let file = rfs::open(&overlay_path, open | OFlags::TRUNC, file_mode)
                        .map(File::from)
                        .map_err(io::Error::from)?;
                    apply_created_file_identity(&file, uid, gid, file_mode)?;
                    if !self.config.relaxed_create_durability {
                        file.sync_data()?;
                        sync_directory(overlay_path.parent().ok_or_else(|| {
                            io::Error::new(ErrorKind::InvalidInput, "overlay has no parent")
                        })?)?;
                    }
                    Ok(file)
                })();
                overlay_result.map_or_else(
                    |_| {
                        let _ = fs::remove_file(&overlay_path);
                        (canonical, false)
                    },
                    |file| (file, true),
                )
            };

            let attr = self.attr_for_path(&logical)?;
            let backing = Arc::new(reply.open_backing(&file)?);
            let fh = self.insert_file_handle(
                attr.ino,
                OpenHandle::new(
                    logical,
                    HandleData::File(file),
                    true,
                    overlay,
                    Some(Arc::clone(&backing)),
                    None,
                ),
            );
            Ok((attr, fh, backing))
        })();
        match result {
            Ok((attr, fh, backing)) => {
                self.remember_lookup(attr.ino);
                reply.created_passthrough(
                    &TTL,
                    &attr,
                    FuseGeneration(0),
                    fh,
                    FopenFlags::FOPEN_PARALLEL_DIRECT_WRITES,
                    &backing,
                );
            }
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn fallocate(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        length: u64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        let result = (|| -> io::Result<()> {
            let file = {
                let handles = self.handles.lock();
                let handle = handles
                    .get(&fh.0)
                    .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
                let HandleData::File(file) = &handle.data else {
                    return Err(io::Error::from_raw_os_error(libc::EBADF));
                };
                let file = file.try_clone()?;
                drop(handles);
                file
            };
            rfs::fallocate(
                &file,
                rfs::FallocateFlags::from_bits_retain(u32::try_from(mode).unwrap_or_default()),
                offset,
                length,
            )
            .map_err(io::Error::from)
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn lseek(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: i64,
        whence: i32,
        reply: fuser::ReplyLseek,
    ) {
        let result = (|| -> io::Result<i64> {
            let file = {
                let handles = self.handles.lock();
                let handle = handles
                    .get(&fh.0)
                    .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
                let HandleData::File(file) = &handle.data else {
                    return Err(io::Error::from_raw_os_error(libc::ESPIPE));
                };
                let file = file.try_clone()?;
                drop(handles);
                file
            };
            let seek = match whence {
                libc::SEEK_SET => rfs::SeekFrom::Start(
                    u64::try_from(offset)
                        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?,
                ),
                libc::SEEK_CUR => rfs::SeekFrom::Current(offset),
                libc::SEEK_END => rfs::SeekFrom::End(offset),
                libc::SEEK_DATA => rfs::SeekFrom::Data(
                    u64::try_from(offset)
                        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?,
                ),
                libc::SEEK_HOLE => rfs::SeekFrom::Hole(
                    u64::try_from(offset)
                        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?,
                ),
                _ => return Err(io::Error::from_raw_os_error(libc::EINVAL)),
            };
            let value = rfs::seek(&file, seek).map_err(io::Error::from)?;
            i64::try_from(value).map_err(|_| io::Error::from_raw_os_error(libc::EOVERFLOW))
        })();
        match result {
            Ok(offset) => reply.offset(offset),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn copy_file_range(
        &self,
        _req: &Request,
        _ino_in: INodeNo,
        fh_in: FileHandle,
        offset_in: u64,
        _ino_out: INodeNo,
        fh_out: FileHandle,
        offset_out: u64,
        len: u64,
        flags: CopyFileRangeFlags,
        reply: ReplyWrite,
    ) {
        if !flags.is_empty() {
            reply.error(Errno::EINVAL);
            return;
        }
        let result = (|| -> io::Result<u32> {
            let (input_file, output_file) = {
                let handles = self.handles.lock();
                let input = handles
                    .get(&fh_in.0)
                    .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
                let output = handles
                    .get(&fh_out.0)
                    .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
                let HandleData::File(input_file) = &input.data else {
                    return Err(io::Error::from_raw_os_error(libc::EBADF));
                };
                let HandleData::File(output_file) = &output.data else {
                    return Err(io::Error::from_raw_os_error(libc::EBADF));
                };
                let input_file = input_file.try_clone()?;
                let output_file = output_file.try_clone()?;
                drop(handles);
                (input_file, output_file)
            };
            let mut input_offset = offset_in;
            let mut output_offset = offset_out;
            let requested = usize::try_from(len).unwrap_or(usize::MAX);
            let copied = rfs::copy_file_range(
                &input_file,
                Some(&mut input_offset),
                &output_file,
                Some(&mut output_offset),
                requested,
            )
            .map_err(io::Error::from)?;
            u32::try_from(copied).map_err(|_| io::Error::from_raw_os_error(libc::EOVERFLOW))
        })();
        match result {
            Ok(copied) => reply.written(copied),
            Err(error) => reply.error(errno(&error)),
        }
    }
}
