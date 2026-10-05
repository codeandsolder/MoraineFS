use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use walkdir::WalkDir;

use crate::paths::Layout;

static TEMP_SERIAL: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Generation {
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

impl Generation {
    pub fn from_metadata(metadata: &fs::Metadata) -> io::Result<Self> {
        Ok(Self {
            size: metadata.len(),
            mtime_ns: nanoseconds(metadata.mtime(), metadata.mtime_nsec())?,
            ctime_ns: nanoseconds(metadata.ctime(), metadata.ctime_nsec())?,
        })
    }
}

fn nanoseconds(seconds: i64, nanos: i64) -> io::Result<i64> {
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(nanos))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "timestamp overflows i64 ns"))
}

pub trait MetadataStore: Send + Sync {
    fn read_generation(&self, source: &Path) -> io::Result<Option<Generation>>;
    fn write_generation(&self, source: &Path, generation: Generation) -> io::Result<()>;
    fn remove_generation(&self, source: &Path) -> io::Result<()>;
    fn sources(&self) -> io::Result<Vec<PathBuf>>;
}

#[derive(Debug, Clone)]
pub struct FileMetadataStore {
    layout: Layout,
    root: PathBuf,
}

impl FileMetadataStore {
    #[must_use]
    pub const fn new(layout: Layout, root: PathBuf) -> Self {
        Self { layout, root }
    }

    fn path_for(&self, source: &Path) -> io::Result<PathBuf> {
        self.layout.adapter_path(&self.root, source, b".state")
    }
}

impl MetadataStore for FileMetadataStore {
    fn read_generation(&self, source: &Path) -> io::Result<Option<Generation>> {
        let path = self.path_for(source)?;
        let Ok(raw) = fs::read_to_string(path) else {
            return Ok(None);
        };
        let mut fields = raw.split_ascii_whitespace();
        let Some(size) = fields.next() else {
            return Ok(None);
        };
        let Some(mtime_ns) = fields.next() else {
            return Ok(None);
        };
        let Some(ctime_ns) = fields.next() else {
            return Ok(None);
        };
        if fields.next().is_some() {
            return Ok(None);
        }
        let parsed = (|| {
            Some(Generation {
                size: size.parse().ok()?,
                mtime_ns: mtime_ns.parse().ok()?,
                ctime_ns: ctime_ns.parse().ok()?,
            })
        })();
        Ok(parsed)
    }

    fn write_generation(&self, source: &Path, generation: Generation) -> io::Result<()> {
        let path = self.path_for(source)?;
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "state path has no parent")
        })?;
        fs::create_dir_all(parent)?;

        let serial = TEMP_SERIAL.fetch_add(1, Ordering::Relaxed);
        let file_name = path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "state path has no file name")
        })?;
        let mut temp_name = Vec::with_capacity(file_name.as_bytes().len() + 48);
        temp_name.push(b'.');
        temp_name.extend_from_slice(file_name.as_bytes());
        temp_name.extend_from_slice(format!(".tmp.{}.{serial}", std::process::id()).as_bytes());
        let temp = parent.join(OsString::from_vec(temp_name));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        writeln!(
            file,
            "{} {} {}",
            generation.size, generation.mtime_ns, generation.ctime_ns
        )?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, &path)?;
        sync_directory(parent)
    }

    fn remove_generation(&self, source: &Path) -> io::Result<()> {
        remove_if_exists(&self.path_for(source)?)
    }

    fn sources(&self) -> io::Result<Vec<PathBuf>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut sources = Vec::new();
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
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().as_bytes();
            let Some(base) = name.strip_suffix(b".state") else {
                continue;
            };
            let relative = entry.path().strip_prefix(&self.root).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "state path outside metadata root",
                )
            })?;
            let key = relative.with_file_name(OsString::from_vec(base.to_vec()));
            if let Ok(source) = self.layout.source_from_storage_key(&key) {
                sources.push(source);
            }
        }
        sources.sort();
        sources.dedup();
        Ok(sources)
    }
}

pub fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

pub fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
