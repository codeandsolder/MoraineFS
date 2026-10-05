use std::fs;
use std::io;
use std::path::Path;

use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RangeGcStats {
    pub removed_dirs: u64,
    pub removed_bytes: u64,
    pub active_dirs: u64,
}

pub fn collect_stale_process_dirs(root: &Path) -> io::Result<RangeGcStats> {
    fs::create_dir_all(root)?;
    let mut stats = RangeGcStats::default();
    for entry in fs::read_dir(root)? {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(raw_pid) = name.to_str().and_then(|name| name.strip_prefix("pid-")) else {
            continue;
        };
        let Ok(pid) = raw_pid.parse::<u32>() else {
            continue;
        };
        if Path::new("/proc").join(pid.to_string()).exists() {
            stats.active_dirs = stats.active_dirs.saturating_add(1);
            continue;
        }
        let mut bytes = 0_u64;
        for child in WalkDir::new(&path)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            if child.file_type().is_file()
                && let Ok(metadata) = child.metadata()
            {
                bytes = bytes.saturating_add(metadata.len());
            }
        }
        fs::remove_dir_all(&path)?;
        stats.removed_dirs = stats.removed_dirs.saturating_add(1);
        stats.removed_bytes = stats.removed_bytes.saturating_add(bytes);
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::fs;
    use std::os::unix::fs::symlink;

    use tempfile::TempDir;

    use super::collect_stale_process_dirs;

    #[test]
    fn removes_stale_process_tree_and_counts_bytes() {
        let temp = TempDir::new().unwrap();
        let stale = temp.path().join("pid-4294967295");
        fs::create_dir_all(stale.join("nested")).unwrap();
        fs::write(stale.join("a"), b"abc").unwrap();
        fs::write(stale.join("nested/b"), b"12345").unwrap();

        let stats = collect_stale_process_dirs(temp.path()).unwrap();
        assert_eq!(stats.removed_dirs, 1);
        assert_eq!(stats.removed_bytes, 8);
        assert_eq!(stats.active_dirs, 0);
        assert!(!stale.exists());
    }

    #[test]
    fn keeps_active_process_and_ignores_non_process_entries() {
        let temp = TempDir::new().unwrap();
        let active = temp.path().join(format!("pid-{}", std::process::id()));
        let unrelated = temp.path().join("other");
        fs::create_dir_all(&active).unwrap();
        fs::create_dir_all(&unrelated).unwrap();

        let stats = collect_stale_process_dirs(temp.path()).unwrap();
        assert_eq!(stats.removed_dirs, 0);
        assert_eq!(stats.active_dirs, 1);
        assert!(active.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn does_not_follow_top_level_pid_symlink() {
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("target");
        let link = temp.path().join("pid-4294967294");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("keep"), b"safe").unwrap();
        symlink(&target, &link).unwrap();

        let stats = collect_stale_process_dirs(temp.path()).unwrap();
        assert_eq!(stats.removed_dirs, 0);
        assert_eq!(fs::read(target.join("keep")).unwrap(), b"safe");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    }
}
