use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Layout {
    pub overlay_root: PathBuf,
    pub source_root: PathBuf,
}

impl Layout {
    /// Validate a source path and return its temporary file-adapter key.
    ///
    /// The directory-backed prototype adapters currently key entries by the
    /// full absolute source path below their private roots. This encoding is an
    /// implementation detail, not a stable on-disk format.
    pub(crate) fn storage_key<'a>(&self, source: &'a Path) -> io::Result<&'a Path> {
        if !source.is_absolute() || !source.starts_with(&self.source_root) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source path is outside source root",
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

    pub(crate) fn source_from_storage_key(&self, key: &Path) -> io::Result<PathBuf> {
        if key.is_absolute()
            || key
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "storage key is not normalized relative data",
            ));
        }
        let source = Path::new("/").join(key);
        self.storage_key(&source)?;
        Ok(source)
    }

    pub(crate) fn adapter_path(
        &self,
        root: &Path,
        source: &Path,
        suffix: &[u8],
    ) -> io::Result<PathBuf> {
        let key = self.storage_key(source)?;
        if suffix.is_empty() {
            return Ok(root.join(key));
        }
        let parent = key.parent().unwrap_or_else(|| Path::new(""));
        let name = key.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "source has no final component")
        })?;
        let mut bytes = name.as_bytes().to_vec();
        bytes.extend_from_slice(suffix);
        Ok(root
            .join(parent)
            .join(OsString::from(OsStr::from_bytes(&bytes))))
    }

    pub fn overlay_path(&self, source: &Path) -> io::Result<PathBuf> {
        self.adapter_path(&self.overlay_root, source, b"")
    }

    pub(crate) fn transaction_root(&self) -> PathBuf {
        self.overlay_root.join(".morainefs-transactions")
    }

    pub(crate) fn rename_backup_path(&self, source: &Path) -> io::Result<PathBuf> {
        self.adapter_path(&self.transaction_root(), source, b".rename-backup")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::{Path, PathBuf};

    use super::Layout;

    fn layout() -> Layout {
        Layout {
            overlay_root: PathBuf::from("/private/overlay"),
            source_root: PathBuf::from("/srv/scratch"),
        }
    }

    #[test]
    fn prototype_adapter_keys_are_internal_details() {
        let layout = layout();
        let source = Path::new("/srv/scratch/project/file");
        assert_eq!(
            layout.overlay_path(source).unwrap(),
            Path::new("/private/overlay/srv/scratch/project/file")
        );
        assert_eq!(
            layout
                .source_from_storage_key(Path::new("srv/scratch/project/file"))
                .unwrap(),
            source
        );
    }

    #[test]
    fn rejects_paths_outside_source_root_or_with_escape_components() {
        let layout = layout();
        assert!(layout.overlay_path(Path::new("/srv/other/file")).is_err());
        assert!(
            layout
                .overlay_path(Path::new("/srv/scratch/../secret"))
                .is_err()
        );
        assert!(
            layout
                .source_from_storage_key(Path::new("srv/scratch/../secret"))
                .is_err()
        );
    }
}
