use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags, open};

use crate::paths::Layout;
use crate::store::{remove_if_exists, sync_directory};

pub trait NamespaceJournal: Send + Sync {
    fn create_marker_exists(&self, source: &Path) -> io::Result<bool>;
    fn clear_create_marker(&self, source: &Path) -> io::Result<()>;
    fn rename_marker_exists(&self, source: &Path) -> io::Result<bool>;
    fn rename_ready_exists(&self, source: &Path) -> io::Result<bool>;
    fn read_rename_source(&self, destination: &Path) -> io::Result<Option<PathBuf>>;
    fn mark_rename_ready(&self, destination: &Path) -> io::Result<()>;
    fn clear_rename_ready(&self, destination: &Path) -> io::Result<()>;
    fn clear_rename_marker(&self, destination: &Path) -> io::Result<()>;
    fn remove_destination_backup(&self, destination: &Path) -> io::Result<()>;
    fn destination_backup_path(&self, destination: &Path) -> io::Result<PathBuf>;
    fn rename_root(&self) -> &Path;
}

#[derive(Debug, Clone)]
pub struct FileNamespaceJournal {
    layout: Layout,
}

impl FileNamespaceJournal {
    #[must_use]
    pub const fn new(layout: Layout) -> Self {
        Self { layout }
    }

    fn marker_is_file(path: &Path) -> bool {
        fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
    }

    fn validate_rename_source(&self, raw: Vec<u8>, destination: &Path) -> Option<PathBuf> {
        if raw.is_empty() || raw.contains(&0) {
            return None;
        }
        let path = PathBuf::from(OsString::from_vec(raw));
        if !path.is_absolute()
            || !path.starts_with(&self.layout.source_prefix)
            || path == destination
            || !is_normalized_absolute(&path)
        {
            return None;
        }
        Some(path)
    }
}

impl NamespaceJournal for FileNamespaceJournal {
    fn create_marker_exists(&self, source: &Path) -> io::Result<bool> {
        Ok(Self::marker_is_file(
            &self.layout.create_marker_path(source)?,
        ))
    }

    fn clear_create_marker(&self, source: &Path) -> io::Result<()> {
        let path = self.layout.create_marker_path(source)?;
        remove_and_sync_parent(&path)
    }

    fn rename_marker_exists(&self, source: &Path) -> io::Result<bool> {
        Ok(Self::marker_is_file(
            &self.layout.rename_marker_path(source)?,
        ))
    }

    fn rename_ready_exists(&self, source: &Path) -> io::Result<bool> {
        Ok(Self::marker_is_file(
            &self.layout.rename_ready_path(source)?,
        ))
    }

    fn read_rename_source(&self, destination: &Path) -> io::Result<Option<PathBuf>> {
        let marker = self.layout.rename_marker_path(destination)?;
        let Ok(raw) = fs::read(marker) else {
            return Ok(None);
        };
        Ok(self.validate_rename_source(raw, destination))
    }

    fn mark_rename_ready(&self, destination: &Path) -> io::Result<()> {
        let path = self.layout.rename_ready_path(destination)?;
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
        let path = self.layout.rename_ready_path(destination)?;
        remove_and_sync_parent(&path)
    }

    fn clear_rename_marker(&self, destination: &Path) -> io::Result<()> {
        let path = self.layout.rename_marker_path(destination)?;
        remove_and_sync_parent(&path)
    }

    fn remove_destination_backup(&self, destination: &Path) -> io::Result<()> {
        let path = self.layout.rename_dest_backup_path(destination)?;
        remove_and_sync_parent(&path)
    }

    fn destination_backup_path(&self, destination: &Path) -> io::Result<PathBuf> {
        self.layout.rename_dest_backup_path(destination)
    }

    fn rename_root(&self) -> &Path {
        &self.layout.rename_root
    }
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
