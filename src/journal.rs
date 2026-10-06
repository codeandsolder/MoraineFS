use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags, open};
use walkdir::WalkDir;

use crate::paths::Layout;
use crate::store::{remove_if_exists, sync_directory};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JournalCleanup {
    pub orphan_ready_pruned: u64,
}

pub trait NamespaceJournal: Send + Sync {
    fn create_marker_exists(&self, source: &Path) -> io::Result<bool>;
    fn mark_created(&self, source: &Path) -> io::Result<()>;
    fn clear_create_marker(&self, source: &Path) -> io::Result<()>;
    fn rename_marker_exists(&self, source: &Path) -> io::Result<bool>;
    fn begin_rename(&self, source: &Path, destination: &Path) -> io::Result<()>;
    fn rename_ready_exists(&self, source: &Path) -> io::Result<bool>;
    fn read_rename_source(&self, destination: &Path) -> io::Result<Option<PathBuf>>;
    fn mark_rename_ready(&self, destination: &Path) -> io::Result<()>;
    fn clear_rename_ready(&self, destination: &Path) -> io::Result<()>;
    fn clear_rename_marker(&self, destination: &Path) -> io::Result<()>;
    fn pending_renames(&self) -> io::Result<Vec<PathBuf>>;
    fn prune_orphans(&self) -> io::Result<JournalCleanup>;
}

#[derive(Debug, Clone)]
pub struct FileNamespaceJournal {
    layout: Layout,
    root: PathBuf,
}

impl FileNamespaceJournal {
    #[must_use]
    pub const fn new(layout: Layout, root: PathBuf) -> Self {
        Self { layout, root }
    }

    fn marker_is_file(path: &Path) -> bool {
        fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
    }

    fn marker_path(&self, source: &Path, suffix: &[u8]) -> io::Result<PathBuf> {
        self.layout.adapter_path(&self.root, source, suffix)
    }

    fn validate_rename_source(&self, raw: Vec<u8>, destination: &Path) -> Option<PathBuf> {
        if raw.is_empty() || raw.contains(&0) {
            return None;
        }
        let path = PathBuf::from(OsString::from_vec(raw));
        if !path.is_absolute()
            || !path.starts_with(&self.layout.source_root)
            || path == destination
            || !is_normalized_absolute(&path)
        {
            return None;
        }
        Some(path)
    }

    fn source_from_marker(&self, marker: &Path, suffix: &[u8]) -> io::Result<PathBuf> {
        let relative = marker.strip_prefix(&self.root).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "marker outside rename root")
        })?;
        let name = relative
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "marker has no file name"))?;
        let base = name
            .as_bytes()
            .strip_suffix(suffix)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "marker suffix mismatch"))?;
        self.layout
            .source_from_storage_key(&relative.with_file_name(OsString::from_vec(base.to_vec())))
    }

    fn collect_suffix_files(&self, suffix: &[u8]) -> io::Result<Vec<PathBuf>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        for entry in WalkDir::new(&self.root).follow_links(false) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    if let Some(io_error) = error.io_error()
                        && io_error.kind() != io::ErrorKind::NotFound
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

    fn prune_suffix(&self, suffix: &[u8], marker_suffix: &[u8]) -> io::Result<u64> {
        let mut removed = 0_u64;
        for path in self.collect_suffix_files(suffix)? {
            let Some(name) = path.file_name() else {
                continue;
            };
            let Some(base) = name.as_bytes().strip_suffix(suffix) else {
                continue;
            };
            let mut marker_name = base.to_vec();
            marker_name.extend_from_slice(marker_suffix);
            let marker = path.with_file_name(OsString::from_vec(marker_name));
            if marker.exists() || fs::remove_file(&path).is_err() {
                continue;
            }
            if let Some(parent) = path.parent() {
                sync_directory(parent)?;
            }
            removed = removed.saturating_add(1);
        }
        Ok(removed)
    }
}

impl NamespaceJournal for FileNamespaceJournal {
    fn create_marker_exists(&self, source: &Path) -> io::Result<bool> {
        Ok(Self::marker_is_file(
            &self.marker_path(source, b".created")?,
        ))
    }

    fn mark_created(&self, source: &Path) -> io::Result<()> {
        write_marker(&self.marker_path(source, b".created")?, b"created-v1\n")
    }

    fn clear_create_marker(&self, source: &Path) -> io::Result<()> {
        remove_and_sync_parent(&self.marker_path(source, b".created")?)
    }

    fn rename_marker_exists(&self, source: &Path) -> io::Result<bool> {
        Ok(Self::marker_is_file(&self.marker_path(source, b".rename")?))
    }

    fn begin_rename(&self, source: &Path, destination: &Path) -> io::Result<()> {
        let ready = self.marker_path(destination, b".rename.ready")?;
        remove_if_exists(&ready)?;
        let marker = self.marker_path(destination, b".rename")?;
        write_marker(&marker, source.as_os_str().as_bytes())
    }

    fn rename_ready_exists(&self, source: &Path) -> io::Result<bool> {
        Ok(Self::marker_is_file(
            &self.marker_path(source, b".rename.ready")?,
        ))
    }

    fn read_rename_source(&self, destination: &Path) -> io::Result<Option<PathBuf>> {
        let marker = self.marker_path(destination, b".rename")?;
        let Ok(raw) = fs::read(marker) else {
            return Ok(None);
        };
        Ok(self.validate_rename_source(raw, destination))
    }

    fn mark_rename_ready(&self, destination: &Path) -> io::Result<()> {
        let path = self.marker_path(destination, b".rename.ready")?;
        let parent = parent(&path)?;
        fs::create_dir_all(parent)?;
        let mut file = open(
            &path,
            OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o600),
        )
        .map(File::from)
        .map_err(io::Error::from)?;
        file.write_all(b"ready-v1\n")?;
        file.sync_all()?;
        drop(file);
        sync_directory(parent)
    }

    fn clear_rename_ready(&self, destination: &Path) -> io::Result<()> {
        remove_and_sync_parent(&self.marker_path(destination, b".rename.ready")?)
    }

    fn clear_rename_marker(&self, destination: &Path) -> io::Result<()> {
        remove_and_sync_parent(&self.marker_path(destination, b".rename")?)
    }

    fn pending_renames(&self) -> io::Result<Vec<PathBuf>> {
        let mut destinations = self
            .collect_suffix_files(b".rename")?
            .into_iter()
            .filter_map(|marker| self.source_from_marker(&marker, b".rename").ok())
            .collect::<Vec<_>>();
        destinations.sort();
        Ok(destinations)
    }

    fn prune_orphans(&self) -> io::Result<JournalCleanup> {
        Ok(JournalCleanup {
            orphan_ready_pruned: self.prune_suffix(b".rename.ready", b".rename")?,
        })
    }
}

fn write_marker(path: &Path, payload: &[u8]) -> io::Result<()> {
    let parent = parent(path)?;
    fs::create_dir_all(parent)?;
    let mut file = open(
        path,
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::from_raw_mode(0o600),
    )
    .map(File::from)
    .map_err(io::Error::from)?;
    file.write_all(payload)?;
    file.sync_all()?;
    drop(file);
    sync_directory(parent)
}

fn remove_and_sync_parent(path: &Path) -> io::Result<()> {
    remove_if_exists(path)?;
    sync_directory(parent(path)?)
}

fn parent(path: &Path) -> io::Result<&Path> {
    path.parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))
}

fn is_normalized_absolute(path: &Path) -> bool {
    let raw = path.as_os_str().as_bytes();
    if raw.first() != Some(&b'/') || (raw.len() > 1 && raw.last() == Some(&b'/')) {
        return false;
    }
    if raw.windows(2).any(|pair| pair == b"//") {
        return false;
    }
    raw.split(|byte| *byte == b'/')
        .skip(1)
        .all(|component| !matches!(component, b"." | b".." | b""))
}
