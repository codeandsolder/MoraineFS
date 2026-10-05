use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Layout {
    pub writeback_root: PathBuf,
    pub state_root: PathBuf,
    pub namespace_root: PathBuf,
    pub rename_root: PathBuf,
    pub source_prefix: PathBuf,
}

impl Layout {
    /// Validate a canonical source path and return its storage-relative form.
    ///
    /// `MoraineFS`'s on-disk ABI mirrors the *full* absolute source path below
    /// each private root. For example `/srv/scratch/a` maps to
    /// `<writeback_root>/srv/scratch/a`; `source_prefix` is an admission
    /// boundary, not the prefix removed from stored paths.
    pub fn relative_source<'a>(&self, source: &'a Path) -> io::Result<&'a Path> {
        if !source.is_absolute() || !source.starts_with(&self.source_prefix) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source path is outside source prefix",
            ));
        }
        if source
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source path is not normalized",
            ));
        }
        source.strip_prefix(Path::new("/")).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "source path must be absolute")
        })
    }

    /// Reconstruct and validate an absolute source path from a private-root
    /// relative path.
    pub fn source_from_relative(&self, relative: &Path) -> io::Result<PathBuf> {
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "private storage path is not normalized relative data",
            ));
        }
        let source = Path::new("/").join(relative);
        self.relative_source(&source)?;
        Ok(source)
    }

    pub fn writeback_path(&self, source: &Path) -> io::Result<PathBuf> {
        Ok(self.writeback_root.join(self.relative_source(source)?))
    }

    pub fn state_path(&self, source: &Path) -> io::Result<PathBuf> {
        self.with_suffix(&self.state_root, source, b".state")
    }

    pub fn create_marker_path(&self, source: &Path) -> io::Result<PathBuf> {
        self.with_suffix(&self.namespace_root, source, b".created")
    }

    pub fn rename_marker_path(&self, source: &Path) -> io::Result<PathBuf> {
        self.with_suffix(&self.rename_root, source, b".rename")
    }

    pub fn rename_ready_path(&self, source: &Path) -> io::Result<PathBuf> {
        self.with_suffix(&self.rename_root, source, b".rename.ready")
    }

    pub fn rename_dest_backup_path(&self, source: &Path) -> io::Result<PathBuf> {
        self.with_suffix(&self.rename_root, source, b".rename.dst-overlay")
    }

    fn with_suffix(&self, root: &Path, source: &Path, suffix: &[u8]) -> io::Result<PathBuf> {
        let relative = self.relative_source(source)?;
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let name = relative.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "source has no final component")
        })?;
        let mut bytes = name.as_bytes().to_vec();
        bytes.extend_from_slice(suffix);
        Ok(root
            .join(parent)
            .join(OsString::from(OsStr::from_bytes(&bytes))))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::{Path, PathBuf};

    use super::Layout;

    fn layout() -> Layout {
        Layout {
            writeback_root: PathBuf::from("/private/writeback"),
            state_root: PathBuf::from("/private/state"),
            namespace_root: PathBuf::from("/private/namespace"),
            rename_root: PathBuf::from("/private/rename"),
            source_prefix: PathBuf::from("/srv/scratch"),
        }
    }

    #[test]
    fn private_paths_preserve_full_absolute_source_layout() {
        let layout = layout();
        let source = Path::new("/srv/scratch/project/file");
        assert_eq!(
            layout.writeback_path(source).unwrap(),
            Path::new("/private/writeback/srv/scratch/project/file")
        );
        assert_eq!(
            layout.state_path(source).unwrap(),
            Path::new("/private/state/srv/scratch/project/file.state")
        );
    }

    #[test]
    fn reconstructs_source_from_private_relative_path() {
        let layout = layout();
        assert_eq!(
            layout
                .source_from_relative(Path::new("srv/scratch/project/file"))
                .unwrap(),
            Path::new("/srv/scratch/project/file")
        );
        assert!(
            layout
                .source_from_relative(Path::new("srv/other/file"))
                .is_err()
        );
    }

    #[test]
    fn rejects_escape_components() {
        let layout = layout();
        assert!(
            layout
                .writeback_path(Path::new("/srv/scratch/../secret"))
                .is_err()
        );
        assert!(
            layout
                .source_from_relative(Path::new("srv/scratch/../secret"))
                .is_err()
        );
    }
}
