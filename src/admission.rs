use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileExt as StdFileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;
use rustix::fs::{
    Advice, Gid, Mode, OFlags, Timespec, Timestamps, Uid, fadvise, fchmod, fchown, futimens, major,
    minor, open,
};
use walkdir::WalkDir;
use xattr::FileExt as XattrFileExt;

use crate::checkpoint::open_read_nofollow_for_admission;

const ORIGIN_XATTR: &str = "user.io_tier.origin_v1";
const COPY_CHUNK: usize = 1024 * 1024;
static TEMP_SERIAL: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OriginFingerprint {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: i64,
    pub ctime_sec: i64,
    pub ctime_nsec: i64,
}

impl OriginFingerprint {
    #[must_use]
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            size: metadata.len(),
            mtime_sec: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
            ctime_sec: metadata.ctime(),
            ctime_nsec: metadata.ctime_nsec(),
        }
    }

    #[must_use]
    fn encode(self) -> [u8; 56] {
        let mut out = [0_u8; 56];
        let fields = [self.dev, self.ino, self.size];
        for (index, value) in fields.into_iter().enumerate() {
            let start = index * 8;
            out[start..start + 8].copy_from_slice(&value.to_ne_bytes());
        }
        let signed = [
            self.mtime_sec,
            self.mtime_nsec,
            self.ctime_sec,
            self.ctime_nsec,
        ];
        for (index, value) in signed.into_iter().enumerate() {
            let start = 24 + index * 8;
            out[start..start + 8].copy_from_slice(&value.to_ne_bytes());
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct AdmissionPolicy {
    pub max_size: u64,
    pub parent_budget: u64,
    pub parent_files: usize,
    pub high_watermark: u64,
    pub low_watermark: u64,
}

#[derive(Debug, Clone, Default)]
pub struct AdmissionStats {
    pub counters: HashMap<String, u64>,
}

impl AdmissionStats {
    fn bump(&mut self, key: &str) {
        let counter = self.counters.entry(key.to_owned()).or_default();
        *counter = counter.saturating_add(1);
    }

    fn add(&mut self, key: &str, value: u64) {
        let counter = self.counters.entry(key.to_owned()).or_default();
        *counter = counter.saturating_add(value);
    }
}

pub enum MicroRead {
    File(File),
    Bytes(Arc<[u8]>),
}

pub trait MicroStore: Send + Sync {
    fn valid(&self, source: &Path, source_metadata: &fs::Metadata) -> io::Result<bool>;
    fn open_valid(
        &self,
        source: &Path,
        source_metadata: &fs::Metadata,
    ) -> io::Result<Option<MicroRead>>;
    fn invalidate(&self, source: &Path) -> io::Result<()>;
    fn store(
        &self,
        source: &Path,
        source_file: &File,
        source_metadata: &fs::Metadata,
    ) -> io::Result<()>;
    fn evict_if_needed(
        &self,
        protect: Option<&Path>,
        high_watermark: u64,
        low_watermark: u64,
    ) -> io::Result<AdmissionStats>;
}

#[derive(Debug)]
pub struct DirectoryMicroStore {
    root: PathBuf,
    mm_stat: Option<PathBuf>,
}

impl DirectoryMicroStore {
    pub fn new(root: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&root)?;
        let device = fs::metadata(&root)?.dev();
        let candidate = PathBuf::from(format!(
            "/sys/dev/block/{}:{}/mm_stat",
            major(device),
            minor(device)
        ));
        let mm_stat = candidate.is_file().then_some(candidate);
        Ok(Self { root, mm_stat })
    }

    fn destination(&self, source: &Path) -> io::Result<PathBuf> {
        if !source.is_absolute() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "source path must be absolute",
            ));
        }
        let relative = source
            .strip_prefix("/")
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "source path must be absolute"))?;
        Ok(self.root.join(relative))
    }

    fn memory_used(&self) -> Option<u64> {
        let path = self.mm_stat.as_ref()?;
        let raw = fs::read_to_string(path).ok()?;
        let raw_value = raw.split_ascii_whitespace().nth(2)?;
        raw_value.parse::<u64>().ok()
    }
}

impl MicroStore for DirectoryMicroStore {
    fn valid(&self, source: &Path, source_metadata: &fs::Metadata) -> io::Result<bool> {
        let destination = self.destination(source)?;
        let Ok(metadata) = fs::metadata(&destination) else {
            return Ok(false);
        };
        if metadata.len() != source_metadata.len() {
            return Ok(false);
        }
        let expected = OriginFingerprint::from_metadata(source_metadata).encode();
        let Ok(origin) = xattr::get(&destination, ORIGIN_XATTR) else {
            return Ok(false);
        };
        Ok(origin.as_deref() == Some(expected.as_slice()))
    }

    fn open_valid(
        &self,
        source: &Path,
        source_metadata: &fs::Metadata,
    ) -> io::Result<Option<MicroRead>> {
        if !self.valid(source, source_metadata)? {
            return Ok(None);
        }
        let file = open(
            self.destination(source)?,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(io::Error::from)?;
        Ok(Some(MicroRead::File(file)))
    }

    fn invalidate(&self, source: &Path) -> io::Result<()> {
        match fs::remove_file(self.destination(source)?) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn store(
        &self,
        source: &Path,
        source_file: &File,
        source_metadata: &fs::Metadata,
    ) -> io::Result<()> {
        let destination = self.destination(source)?;
        let parent = destination.parent().ok_or_else(|| {
            io::Error::new(ErrorKind::InvalidInput, "micro destination has no parent")
        })?;
        fs::create_dir_all(parent)?;
        let serial = TEMP_SERIAL.fetch_add(1, Ordering::Relaxed);
        let name = destination
            .file_name()
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "destination has no name"))?;
        let mut temp_name = Vec::with_capacity(name.as_bytes().len() + 56);
        temp_name.push(b'.');
        temp_name.extend_from_slice(name.as_bytes());
        temp_name.extend_from_slice(
            format!(".morainefs.tmp.{}.{serial}", std::process::id()).as_bytes(),
        );
        let temp = parent.join(OsString::from_vec(temp_name));
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(source_metadata.mode() & 0o7777)
            .open(&temp)?;

        let result = (|| -> io::Result<()> {
            let before = OriginFingerprint::from_metadata(source_metadata);
            let mut buffer = vec![0_u8; COPY_CHUNK];
            let mut offset = 0_u64;
            while offset < before.size {
                let amount = usize::try_from((before.size - offset).min(COPY_CHUNK as u64))
                    .map_err(|_| io::Error::other("micro copy size conversion failed"))?;
                let read = source_file.read_at(&mut buffer[..amount], offset)?;
                if read == 0 {
                    return Err(io::Error::new(ErrorKind::UnexpectedEof, "short micro read"));
                }
                let mut written = 0_usize;
                while written < read {
                    let count = std::io::Write::write(&mut output, &buffer[written..read])?;
                    if count == 0 {
                        return Err(io::Error::new(ErrorKind::WriteZero, "short micro write"));
                    }
                    written += count;
                }
                offset = offset.saturating_add(
                    u64::try_from(read)
                        .map_err(|_| io::Error::other("micro read size conversion failed"))?,
                );
            }

            let after_metadata = source_file.metadata()?;
            let after = OriginFingerprint::from_metadata(&after_metadata);
            if before != after {
                return Err(io::Error::new(
                    ErrorKind::Interrupted,
                    "source changed during micro copy",
                ));
            }
            let _ = fchown(
                &output,
                Some(Uid::from_raw(source_metadata.uid())),
                Some(Gid::from_raw(source_metadata.gid())),
            );
            fchmod(
                &output,
                Mode::from_raw_mode(source_metadata.mode() & 0o7777),
            )
            .map_err(io::Error::from)?;
            let times = Timestamps {
                last_access: Timespec {
                    tv_sec: source_metadata.atime(),
                    tv_nsec: source_metadata.atime_nsec(),
                },
                last_modification: Timespec {
                    tv_sec: source_metadata.mtime(),
                    tv_nsec: source_metadata.mtime_nsec(),
                },
            };
            futimens(&output, &times).map_err(io::Error::from)?;
            output.set_xattr(ORIGIN_XATTR, &after.encode())?;
            fs::rename(&temp, &destination)?;
            if let Ok(file) = File::open(&destination) {
                let _ = fadvise(&file, 0, None, Advice::DontNeed);
            }
            Ok(())
        })();

        drop(output);
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    fn evict_if_needed(
        &self,
        protect: Option<&Path>,
        high_watermark: u64,
        low_watermark: u64,
    ) -> io::Result<AdmissionStats> {
        let before = self.memory_used();
        let mut stats = AdmissionStats::default();
        stats.add("mem_before", before.unwrap_or_default());
        stats.add("mem_after", before.unwrap_or_default());
        if before.is_none_or(|used| used <= high_watermark) {
            return Ok(stats);
        }
        let protected = protect.map(|path| self.destination(path)).transpose()?;
        let mut candidates = Vec::<(i64, PathBuf, Vec<PathBuf>)>::new();
        for entry in WalkDir::new(&self.root)
            .min_depth(1)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_dir())
        {
            let directory = entry.path();
            let mut files = Vec::new();
            let mut newest_atime = i64::MIN;
            let Ok(children) = fs::read_dir(directory) else {
                continue;
            };
            for child in children.filter_map(Result::ok) {
                let path = child.path();
                if path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().contains(".morainefs.tmp."))
                {
                    continue;
                }
                let metadata = match child.metadata() {
                    Ok(metadata) if metadata.is_file() => metadata,
                    _ => continue,
                };
                files.push(path);
                let ns = metadata
                    .atime()
                    .saturating_mul(1_000_000_000)
                    .saturating_add(metadata.atime_nsec());
                newest_atime = newest_atime.max(ns);
            }
            if !files.is_empty() {
                candidates.push((newest_atime, directory.to_path_buf(), files));
            }
        }
        candidates.sort_by_key(|(atime, _, _)| *atime);

        for (_atime, directory, files) in candidates {
            if protected.as_deref() == Some(directory.as_path()) {
                continue;
            }
            for file in files {
                if fs::remove_file(file).is_ok() {
                    stats.bump("evicted_files");
                }
            }
            if fs::remove_dir(&directory).is_ok() {
                stats.bump("evicted_dirs");
            }
            if let Some(used) = self.memory_used() {
                stats.counters.insert("mem_after".to_owned(), used);
                if used <= low_watermark {
                    break;
                }
            }
        }
        if let Some(after) = self.memory_used() {
            stats.counters.insert("mem_after".to_owned(), after);
        }
        Ok(stats)
    }
}

pub struct AdmissionWorker {
    store: Arc<dyn MicroStore>,
    policy: AdmissionPolicy,
    pool: rayon::ThreadPool,
}

impl AdmissionWorker {
    pub fn new(
        store: Arc<dyn MicroStore>,
        policy: AdmissionPolicy,
        workers: usize,
    ) -> io::Result<Self> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers.max(1))
            .thread_name(|index| format!("moraine-admit-{index}"))
            .build()
            .map_err(io::Error::other)?;
        Ok(Self {
            store,
            policy,
            pool,
        })
    }

    pub fn admit_dir(&self, directory: &Path) -> AdmissionStats {
        let mut stats = AdmissionStats::default();
        let Ok(entries) = fs::read_dir(directory) else {
            stats.bump("scan_error");
            return stats;
        };
        let mut candidates = Vec::<(u64, PathBuf)>::new();
        for entry in entries.filter_map(Result::ok) {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.len() <= self.policy.max_size {
                candidates.push((metadata.len(), entry.path()));
            }
        }
        candidates.sort();
        stats.add("candidate_files", candidates.len() as u64);

        let mut selected = Vec::new();
        let mut selected_bytes = 0_u64;
        for (size, path) in candidates {
            if selected.len() >= self.policy.parent_files
                || selected_bytes.saturating_add(size) > self.policy.parent_budget
            {
                break;
            }
            selected.push(path);
            selected_bytes = selected_bytes.saturating_add(size);
        }
        stats.add("selected_files", selected.len() as u64);
        stats.add("selected_bytes", selected_bytes);

        let outcomes = self.pool.install(|| {
            selected
                .par_iter()
                .map(|source| self.copy_one(source))
                .collect::<Vec<_>>()
        });
        for (status, size) in outcomes {
            stats.bump(status);
            if status == "copied" {
                stats.add("copied_bytes", size);
            }
        }
        stats
    }

    pub fn evict_if_needed(&self, protect: Option<&Path>) -> io::Result<AdmissionStats> {
        self.store.evict_if_needed(
            protect,
            self.policy.high_watermark,
            self.policy.low_watermark,
        )
    }

    fn copy_one(&self, source: &Path) -> (&'static str, u64) {
        let Ok(source_file) = open_read_nofollow_for_admission(source) else {
            return ("open_error", 0);
        };
        let Ok(before) = source_file.metadata() else {
            return ("open_error", 0);
        };
        if !before.is_file() || before.len() > self.policy.max_size {
            return ("ineligible", 0);
        }
        match self.store.valid(source, &before) {
            Ok(true) => return ("valid", before.len()),
            Ok(false) => {}
            Err(_) => return ("copy_error", 0),
        }
        match self.store.store(source, &source_file, &before) {
            Ok(()) => ("copied", before.len()),
            Err(error) if error.kind() == ErrorKind::Interrupted => ("changed", 0),
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => ("short_read", 0),
            Err(_) => ("copy_error", 0),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::fs;
    use std::os::unix::fs::{MetadataExt, symlink};
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::{
        AdmissionPolicy, AdmissionWorker, DirectoryMicroStore, MicroStore, OriginFingerprint,
    };

    #[test]
    fn origin_fingerprint_matches_wire_layout_size() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("file");
        fs::write(&path, b"abc").unwrap();
        let metadata = fs::metadata(path).unwrap();
        let fingerprint = OriginFingerprint::from_metadata(&metadata);
        assert_eq!(fingerprint.encode().len(), 56);
        assert_eq!(fingerprint.dev, metadata.dev());
    }

    #[test]
    fn admission_copies_regular_files_and_skips_symlinks() {
        let source_root = TempDir::new().unwrap();
        let micro_root = TempDir::new().unwrap();
        let regular = source_root.path().join("regular");
        let link = source_root.path().join("link");
        fs::write(&regular, b"payload").unwrap();
        symlink(&regular, &link).unwrap();

        let store = Arc::new(DirectoryMicroStore::new(micro_root.path().to_path_buf()).unwrap());
        let worker = AdmissionWorker::new(
            store.clone(),
            AdmissionPolicy {
                max_size: 1024,
                parent_budget: 4096,
                parent_files: 16,
                high_watermark: u64::MAX,
                low_watermark: u64::MAX,
            },
            2,
        )
        .unwrap();

        let stats = worker.admit_dir(source_root.path());
        assert_eq!(stats.counters.get("candidate_files"), Some(&1));
        assert_eq!(stats.counters.get("copied"), Some(&1));
        assert_eq!(
            fs::read(store.destination(&regular).unwrap()).unwrap(),
            b"payload"
        );
        assert!(!store.destination(&link).unwrap().exists());

        let second = worker.admit_dir(source_root.path());
        assert_eq!(second.counters.get("valid"), Some(&1));
    }

    #[test]
    fn missing_origin_xattr_is_a_cache_miss() {
        let source_root = TempDir::new().unwrap();
        let micro_root = TempDir::new().unwrap();
        let source = source_root.path().join("file");
        fs::write(&source, b"same-size").unwrap();
        let store = DirectoryMicroStore::new(micro_root.path().to_path_buf()).unwrap();
        let destination = store.destination(&source).unwrap();
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(&destination, b"same-size").unwrap();

        assert!(
            !store
                .valid(&source, &fs::metadata(&source).unwrap())
                .unwrap()
        );
    }

    #[test]
    fn unreadable_or_invalid_mm_stat_disables_eviction_accounting() {
        let temp = TempDir::new().unwrap();
        let mm_stat = temp.path().join("mm_stat");
        fs::write(&mm_stat, b"not enough").unwrap();
        let store = DirectoryMicroStore {
            root: temp.path().join("micro"),
            mm_stat: Some(mm_stat.clone()),
        };
        assert_eq!(store.memory_used(), None);
        fs::remove_file(&mm_stat).unwrap();
        assert_eq!(store.memory_used(), None);
    }
}
