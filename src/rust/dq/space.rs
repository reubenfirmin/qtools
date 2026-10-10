//! Filesystem capacity, separate from the selected directory totals.
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use serde::Serialize;

use crate::chart;

pub(super) const FREE_COLOR: chart::Color = (190, 207, 221);
pub(super) const USED_COLOR: chart::Color = (126, 133, 145);

#[derive(Debug, Serialize)]
pub(super) struct Space {
    pub capacity_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
    pub available_bytes: u64,
}

impl Space {
    pub fn allocation_bound(root: &Path) -> Option<Self> {
        let space = Self::read(root)?;
        let path = CString::new(root.as_os_str().as_bytes()).ok()?;
        let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // Ext4 does not share regular-file data extents between distinct
        // inodes. Reflink/compressed filesystems need a different bound.
        if unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 { return None; }
        let stats = unsafe { stats.assume_init() };
        (stats.f_type == 0xEF53).then_some(space)
    }

    pub fn read(root: &Path) -> Option<Self> {
        // A subdirectory owns only its scanned bytes, not the filesystem's free
        // space. Resolve '.' and symlinks before comparing the mount boundary.
        let root = fs::canonicalize(root).ok()?;
        if let Some(parent) = root.parent() {
            if fs::metadata(&root).ok()?.dev() == fs::metadata(parent).ok()?.dev() { return None; }
        }
        let path = CString::new(root.as_os_str().as_bytes()).ok()?;
        let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // statvfs initializes the complete output on success; the path is NUL terminated.
        if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 { return None; }
        let stats = unsafe { stats.assume_init() };
        let block_size = if stats.f_frsize == 0 { stats.f_bsize } else { stats.f_frsize };
        let capacity_bytes = stats.f_blocks.saturating_mul(block_size);
        if capacity_bytes == 0 { return None; }
        let free_bytes = stats.f_bfree.saturating_mul(block_size).min(capacity_bytes);
        let available_bytes = stats.f_bavail.saturating_mul(block_size).min(free_bytes);
        Some(Self { capacity_bytes, used_bytes: capacity_bytes - free_bytes, free_bytes, available_bytes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn capacity_is_only_available_for_filesystem_roots_including_resolved_paths() {
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let directory = std::env::temp_dir().join(format!("qtools-space-{}-{unique}", std::process::id()));
        fs::create_dir_all(directory.join("subdir")).unwrap();
        symlink("/", directory.join("root-alias")).unwrap();
        symlink(directory.join("subdir"), directory.join("subdir-alias")).unwrap();
        assert!(Space::read(&directory).is_none());
        assert!(Space::read(&directory.join("subdir")).is_none());
        assert!(Space::read(&directory.join("subdir-alias")).is_none());
        let capacity = Space::read(Path::new("/")).unwrap().capacity_bytes;
        assert_eq!(Space::read(&directory.join("root-alias")).unwrap().capacity_bytes, capacity);
        assert!(Space::read(&directory.join("missing")).is_none());
        fs::remove_dir_all(directory).unwrap();
    }
}
