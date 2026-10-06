use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, ErrorKind};
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

use fuser::RenameFlags as FuseRenameFlags;
use rustix::fs::{
    self as rfs, AtFlags, Mode, OFlags, RawDir, RenameFlags, ResolveFlags, linkat, mkdirat,
    openat2, readlinkat, renameat, renameat_with, symlinkat, unlinkat,
};

#[derive(Debug)]
pub(super) struct SourceRoot {
    pub(super) path: PathBuf,
    pub(super) fd: File,
}

pub(super) struct AnchoredPath {
    _parent: File,
    pub(super) path: PathBuf,
}

impl SourceRoot {
    pub(super) fn new(path: PathBuf) -> io::Result<Self> {
        let path = fs::canonicalize(path)?;
        let fd = File::from(
            rfs::open(
                &path,
                OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io::Error::from)?,
        );
        Ok(Self { path, fd })
    }

    pub(super) fn relative<'a>(&self, logical: &'a Path) -> io::Result<&'a Path> {
        if !logical.is_absolute() || !logical.starts_with(&self.path) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "path is outside MoraineFS source root",
            ));
        }
        if logical
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
        {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "path is not normalized",
            ));
        }
        logical
            .strip_prefix(&self.path)
            .map(|path| {
                if path.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    path
                }
            })
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "path is outside source root"))
    }

    pub(super) fn open_raw(&self, logical: &Path, flags: OFlags, mode: Mode) -> io::Result<File> {
        let relative = self.relative(logical)?;
        openat2(
            &self.fd,
            relative,
            flags | OFlags::CLOEXEC,
            mode,
            ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS,
        )
        .map(File::from)
        .map_err(io::Error::from)
    }

    pub(super) fn path_handle(&self, logical: &Path) -> io::Result<File> {
        self.open_raw(logical, OFlags::PATH | OFlags::NOFOLLOW, Mode::empty())
    }

    pub(super) fn metadata(&self, logical: &Path) -> io::Result<fs::Metadata> {
        self.path_handle(logical)?.metadata()
    }

    pub(super) fn open_file(&self, logical: &Path, flags: OFlags, mode: Mode) -> io::Result<File> {
        self.open_raw(logical, flags | OFlags::NOFOLLOW, mode)
    }

    pub(super) fn open_parent(&self, logical_parent: &Path) -> io::Result<File> {
        self.open_raw(
            logical_parent,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW,
            Mode::empty(),
        )
    }

    pub(super) fn anchored_path(&self, logical: &Path) -> io::Result<AnchoredPath> {
        let parent = logical
            .parent()
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "path has no parent"))?;
        let name = logical
            .file_name()
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "path has no name"))?;
        let parent = self.open_parent(parent)?;
        let mut path = PathBuf::from("/proc/self/fd");
        path.push(parent.as_raw_fd().to_string());
        path.push(name);
        Ok(AnchoredPath {
            _parent: parent,
            path,
        })
    }

    pub(super) fn child(parent: &Path, name: &OsStr) -> io::Result<PathBuf> {
        if name.is_empty()
            || name.as_bytes().contains(&b'/')
            || matches!(name.as_bytes(), b"." | b"..")
        {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "invalid child name",
            ));
        }
        Ok(parent.join(name))
    }

    pub(super) fn create_file(
        &self,
        parent: &Path,
        name: &OsStr,
        flags: OFlags,
        mode: Mode,
    ) -> io::Result<File> {
        let dir = self.open_parent(parent)?;
        rfs::openat(
            &dir,
            name,
            flags | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            mode,
        )
        .map(File::from)
        .map_err(io::Error::from)
    }

    pub(super) fn mkdir(&self, parent: &Path, name: &OsStr, mode: Mode) -> io::Result<()> {
        let dir = self.open_parent(parent)?;
        mkdirat(&dir, name, mode).map_err(io::Error::from)
    }

    pub(super) fn unlink(&self, parent: &Path, name: &OsStr, directory: bool) -> io::Result<()> {
        let dir = self.open_parent(parent)?;
        let flags = if directory {
            AtFlags::REMOVEDIR
        } else {
            AtFlags::empty()
        };
        unlinkat(&dir, name, flags).map_err(io::Error::from)
    }

    pub(super) fn symlink(&self, parent: &Path, name: &OsStr, target: &Path) -> io::Result<()> {
        let dir = self.open_parent(parent)?;
        symlinkat(target, &dir, name).map_err(io::Error::from)
    }

    pub(super) fn hard_link(
        &self,
        source: &Path,
        new_parent: &Path,
        new_name: &OsStr,
    ) -> io::Result<()> {
        let source_parent = source
            .parent()
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "source has no parent"))?;
        let source_name = source
            .file_name()
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "source has no name"))?;
        let old_dir = self.open_parent(source_parent)?;
        let new_dir = self.open_parent(new_parent)?;
        linkat(&old_dir, source_name, &new_dir, new_name, AtFlags::empty()).map_err(io::Error::from)
    }

    pub(super) fn rename(
        &self,
        old_parent: &Path,
        old_name: &OsStr,
        new_parent: &Path,
        new_name: &OsStr,
        flags: FuseRenameFlags,
    ) -> io::Result<()> {
        let old_dir = self.open_parent(old_parent)?;
        let new_dir = self.open_parent(new_parent)?;
        if flags.is_empty() {
            return renameat(&old_dir, old_name, &new_dir, new_name).map_err(io::Error::from);
        }
        let mut rustix_flags = RenameFlags::empty();
        if flags.contains(FuseRenameFlags::RENAME_NOREPLACE) {
            rustix_flags |= RenameFlags::NOREPLACE;
        }
        if flags.contains(FuseRenameFlags::RENAME_EXCHANGE) {
            rustix_flags |= RenameFlags::EXCHANGE;
        }
        if flags.contains(FuseRenameFlags::RENAME_WHITEOUT) {
            rustix_flags |= RenameFlags::WHITEOUT;
        }
        renameat_with(&old_dir, old_name, &new_dir, new_name, rustix_flags).map_err(io::Error::from)
    }

    pub(super) fn readlink(&self, logical: &Path) -> io::Result<Vec<u8>> {
        let parent = logical
            .parent()
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "link has no parent"))?;
        let name = logical
            .file_name()
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "link has no name"))?;
        let dir = self.open_parent(parent)?;
        Ok(readlinkat(&dir, name, Vec::new())
            .map_err(io::Error::from)?
            .into_bytes())
    }

    pub(super) fn list(&self, logical: &Path) -> io::Result<Vec<OsString>> {
        let dir = self.open_raw(
            logical,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
            Mode::empty(),
        )?;
        let mut buffer = vec![MaybeUninit::uninit(); 64 * 1024];
        let mut iterator = RawDir::new(&dir, &mut buffer);
        let mut names = Vec::new();
        while let Some(entry) = iterator.next() {
            let entry = entry.map_err(io::Error::from)?;
            let bytes = entry.file_name().to_bytes();
            if matches!(bytes, b"." | b"..") {
                continue;
            }
            names.push(OsString::from_vec(bytes.to_vec()));
        }
        Ok(names)
    }
}
