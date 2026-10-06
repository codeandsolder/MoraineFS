use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

const MIN_BLOCK: u64 = 64 * 1024;
const MID_BLOCK: u64 = 256 * 1024;
const SEQ_BLOCK: u64 = 1024 * 1024;
const MAX_BLOCK: u64 = 4 * 1024 * 1024;
const SEQ_GAP: u64 = 64 * 1024;
const COPY_CHUNK: usize = 256 * 1024;
static TEMP_SERIAL: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fingerprint {
    dev: u64,
    ino: u64,
    size: u64,
    mtime_sec: i64,
    mtime_nsec: i64,
    ctime_sec: i64,
    ctime_nsec: i64,
}

impl Fingerprint {
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
}

#[derive(Debug, Clone)]
struct CacheEntry {
    size: u64,
    last_use: u64,
}

#[derive(Debug, Default)]
struct CacheState {
    entries: HashMap<PathBuf, CacheEntry>,
    bytes: u64,
    clock: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct RangeReadState {
    last_end: Option<u64>,
    sequential_streak: u32,
    block_size: u64,
}

impl Default for RangeReadState {
    fn default() -> Self {
        Self {
            last_end: None,
            sequential_streak: 0,
            block_size: MIN_BLOCK,
        }
    }
}

impl RangeReadState {
    fn block_size_for(&mut self, offset: u64, size: u64) -> u64 {
        if self
            .last_end
            .is_some_and(|last| offset >= last && offset - last <= SEQ_GAP)
        {
            self.sequential_streak = self.sequential_streak.saturating_add(1);
        } else {
            self.sequential_streak = 0;
            self.block_size = MIN_BLOCK;
        }
        self.block_size = match self.sequential_streak {
            0 => self.block_size,
            1 => self.block_size.max(MID_BLOCK),
            2..=3 => self.block_size.max(SEQ_BLOCK),
            _ => MAX_BLOCK,
        };
        self.last_end = offset.checked_add(size);
        self.block_size
    }
}

#[derive(Debug)]
pub struct RangeCache {
    root: PathBuf,
    capacity: u64,
    fill: Mutex<()>,
    state: Mutex<CacheState>,
}

impl RangeCache {
    pub fn new(root: PathBuf, capacity: u64) -> io::Result<Self> {
        fs::create_dir_all(root.join(format!("pid-{}", std::process::id())))?;
        Ok(Self {
            root,
            capacity,
            fill: Mutex::new(()),
            state: Mutex::new(CacheState::default()),
        })
    }

    pub fn read(
        &self,
        source: &File,
        offset: u64,
        size: usize,
        state: &mut RangeReadState,
    ) -> io::Result<Option<Vec<u8>>> {
        if size == 0 {
            return Ok(Some(Vec::new()));
        }
        let before_metadata = source.metadata()?;
        if !before_metadata.is_file() || before_metadata.len() == 0 {
            return Ok(None);
        }
        if offset >= before_metadata.len() {
            return Ok(Some(Vec::new()));
        }
        let requested = u64::try_from(size)
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "read size overflow"))?;
        let block_size = state.block_size_for(offset, requested);
        let block_start = offset / block_size * block_size;
        let block_end = block_start.saturating_add(block_size);
        let requested_end = offset.saturating_add(requested);
        if requested_end > block_end {
            return Ok(None);
        }

        let block_len = (before_metadata.len() - block_start).min(block_size);
        let path = self.block_path(&before_metadata, block_start);
        let cache = self.open_or_fill(source, &before_metadata, &path, block_start, block_len)?;
        let available = before_metadata.len().saturating_sub(offset);
        let reply_len = usize::try_from(available.min(requested))
            .map_err(|_| io::Error::other("range reply size overflow"))?;
        let mut output = vec![0_u8; reply_len];
        let cache_offset = offset - block_start;
        let mut done = 0_usize;
        while done < output.len() {
            let read = cache.read_at(
                &mut output[done..],
                cache_offset
                    + u64::try_from(done).map_err(|_| io::Error::other("range offset overflow"))?,
            )?;
            if read == 0 {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "short range-cache read",
                ));
            }
            done += read;
        }
        Ok(Some(output))
    }

    fn block_path(&self, metadata: &fs::Metadata, block_start: u64) -> PathBuf {
        let mtime_ns =
            i128::from(metadata.mtime()) * 1_000_000_000_i128 + i128::from(metadata.mtime_nsec());
        let ctime_ns =
            i128::from(metadata.ctime()) * 1_000_000_000_i128 + i128::from(metadata.ctime_nsec());
        self.root
            .join(format!("pid-{}", std::process::id()))
            .join(format!(
                "{:x}-{:x}-{:x}-{:x}-{:x}",
                metadata.dev(),
                metadata.ino(),
                metadata.len(),
                mtime_ns,
                ctime_ns
            ))
            .join(format!("{block_start:016x}.blk"))
    }

    fn open_or_fill(
        &self,
        source: &File,
        before: &fs::Metadata,
        path: &Path,
        block_start: u64,
        block_len: u64,
    ) -> io::Result<File> {
        if let Some(file) = valid_cached(path, block_len)? {
            self.touch(path, block_len);
            return Ok(file);
        }

        let _guard = self.fill.lock();
        if let Some(file) = valid_cached(path, block_len)? {
            self.touch(path, block_len);
            return Ok(file);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let serial = TEMP_SERIAL.fetch_add(1, Ordering::Relaxed);
        let temp = path.with_extension(format!("tmp.{}.{serial}", std::process::id()));
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        let fill = (|| -> io::Result<()> {
            let mut buffer = vec![0_u8; COPY_CHUNK];
            let mut done = 0_u64;
            while done < block_len {
                let amount = usize::try_from((block_len - done).min(COPY_CHUNK as u64))
                    .map_err(|_| io::Error::other("range block chunk overflow"))?;
                let read = source.read_at(&mut buffer[..amount], block_start + done)?;
                if read == 0 {
                    return Err(io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "short range-cache fill",
                    ));
                }
                output.write_all(&buffer[..read])?;
                done = done.saturating_add(
                    u64::try_from(read).map_err(|_| io::Error::other("range fill overflow"))?,
                );
            }
            let after = source.metadata()?;
            if Fingerprint::from_metadata(before) != Fingerprint::from_metadata(&after) {
                return Err(io::Error::from_raw_os_error(libc::ESTALE));
            }
            output.sync_data()?;
            drop(output);
            fs::rename(&temp, path)?;
            Ok(())
        })();
        if let Err(error) = fill {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }
        self.touch(path, block_len);
        File::open(path)
    }

    fn touch(&self, path: &Path, size: u64) {
        let victims = {
            let mut state = self.state.lock();
            state.clock = state.clock.saturating_add(1);
            let clock = state.clock;
            if let Some(entry) = state.entries.get_mut(path) {
                entry.last_use = clock;
                return;
            }
            state.bytes = state.bytes.saturating_add(size);
            state.entries.insert(
                path.to_path_buf(),
                CacheEntry {
                    size,
                    last_use: clock,
                },
            );
            let mut victims = Vec::new();
            while state.bytes > self.capacity {
                let Some(victim) = state
                    .entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.last_use)
                    .map(|(path, _)| path.clone())
                else {
                    break;
                };
                if let Some(entry) = state.entries.remove(&victim) {
                    state.bytes = state.bytes.saturating_sub(entry.size);
                    victims.push(victim);
                }
            }
            drop(state);
            victims
        };
        for victim in victims {
            let _ = fs::remove_file(victim);
        }
    }
}

fn valid_cached(path: &Path, expected_len: u64) -> io::Result<Option<File>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if metadata.is_file() && metadata.len() == expected_len {
        Ok(Some(file))
    } else {
        drop(file);
        let _ = fs::remove_file(path);
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::fs::{self, File};

    use tempfile::TempDir;

    use super::{RangeCache, RangeReadState};

    #[test]
    fn caches_and_reuses_blocks() {
        let temp = TempDir::new().unwrap();
        let source_path = temp.path().join("source");
        fs::write(&source_path, vec![0x5a; 200 * 1024]).unwrap();
        let source = File::open(&source_path).unwrap();
        let cache = RangeCache::new(temp.path().join("cache"), 1024 * 1024).unwrap();
        let mut state = RangeReadState::default();
        let first = cache.read(&source, 0, 4096, &mut state).unwrap().unwrap();
        let second = cache
            .read(&source, 4096, 4096, &mut state)
            .unwrap()
            .unwrap();
        assert_eq!(first, vec![0x5a; 4096]);
        assert_eq!(second, vec![0x5a; 4096]);
        assert!(
            temp.path()
                .join("cache")
                .join(format!("pid-{}", std::process::id()))
                .exists()
        );
    }

    #[test]
    fn resets_adaptive_block_size_after_random_seek() {
        let mut state = RangeReadState::default();
        assert_eq!(state.block_size_for(0, 4096), 64 * 1024);
        assert_eq!(state.block_size_for(4096, 4096), 256 * 1024);
        assert_eq!(state.block_size_for(8192, 4096), 1024 * 1024);
        assert_eq!(state.block_size_for(16 * 1024 * 1024, 4096), 64 * 1024);
    }
}
