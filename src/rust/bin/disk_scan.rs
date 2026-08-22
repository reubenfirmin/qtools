//! Concurrent filesystem fact collection, independent of accounting and presentation.
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use threadpool::ThreadPool;
use crate::progress::Progress;

#[derive(Debug, Clone)] pub struct FileFact { pub path: PathBuf, pub apparent: u64, pub allocated: u64, pub device: u64,
    pub inode: u64, pub links: u64, pub uid: u32, pub modified: i64 }
#[derive(Debug, Clone)] pub struct DirectoryFact { pub path: PathBuf, pub allocated: u64, pub uid: u32 }
#[derive(Debug)] pub struct Scan { pub root: PathBuf, pub files: Vec<FileFact>, pub directories: Vec<DirectoryFact>,
    pub errors: Vec<String>, pub duration_ms: u128 }
struct Batch { directory: DirectoryFact, subdirs: Vec<PathBuf>, files: Vec<FileFact>, errors: Vec<String>, scanned: u64 }

pub fn scan(root: &Path, threads: usize, one_file_system: bool, excludes: &[String], show_progress: bool) -> Scan {
    let started = std::time::Instant::now();
    let root_device = fs::metadata(root).map(|metadata| metadata.dev()).unwrap_or(0);
    let pool = ThreadPool::new(threads); let (tx, rx) = std::sync::mpsc::channel(); let mut pending = 1usize;
    submit(root.to_path_buf(), root.to_path_buf(), root_device, one_file_system, excludes.to_vec(), &pool, tx.clone());
    let mut progress = Progress::new(show_progress); let mut files = Vec::new(); let mut directories = Vec::new(); let mut errors = Vec::new();
    while pending > 0 {
        let batch = match rx.recv() { Ok(batch) => batch, Err(_) => { errors.push("scan worker channel closed unexpectedly".into()); break } };
        pending -= 1; progress.update(pending, batch.scanned); directories.push(batch.directory); files.extend(batch.files); errors.extend(batch.errors);
        for directory in batch.subdirs { pending += 1; submit(directory, root.to_path_buf(), root_device, one_file_system,
            excludes.to_vec(), &pool, tx.clone()); }
    }
    progress.finish();
    Scan { root: root.to_path_buf(), files, directories, errors, duration_ms: started.elapsed().as_millis() }
}

fn submit(path: PathBuf, root: PathBuf, root_device: u64, one_file_system: bool, excludes: Vec<String>, pool: &ThreadPool, tx: Sender<Batch>) {
    pool.execute(move || { let _ = tx.send(read_directory(path, &root, root_device, one_file_system, &excludes)); });
}
fn read_directory(directory: PathBuf, root: &Path, root_device: u64, one_file_system: bool, excludes: &[String]) -> Batch {
    let metadata = fs::symlink_metadata(&directory).ok();
    let directory_fact = DirectoryFact { path: directory.clone(),
        allocated: metadata.as_ref().map_or(0, |item| item.blocks().saturating_mul(512)),
        uid: metadata.as_ref().map_or(0, |item| item.uid()) };
    let mut batch = Batch { directory: directory_fact, subdirs: Vec::new(), files: Vec::new(), errors: Vec::new(), scanned: 0 };
    let entries = match fs::read_dir(&directory) { Ok(entries) => entries,
        Err(error) => { batch.errors.push(format!("{}: {error}", directory.display())); return batch } };
    for result in entries {
        let entry = match result { Ok(entry) => entry, Err(error) => { batch.errors.push(format!("{}: {error}", directory.display())); continue } };
        let path = entry.path(); let relative = path.strip_prefix(root).unwrap_or(&path).to_string_lossy();
        if excluded(&relative, excludes) || excluded(&entry.file_name().to_string_lossy(), excludes) { continue; }
        let file_type = match entry.file_type() { Ok(kind) => kind, Err(error) => { batch.errors.push(format!("{}: {error}", path.display())); continue } };
        if file_type.is_symlink() { continue; }
        let metadata = match entry.metadata() { Ok(metadata) => metadata, Err(error) => { batch.errors.push(format!("{}: {error}", path.display())); continue } };
        if metadata.is_dir() { if should_descend(one_file_system, root_device, metadata.dev()) { batch.subdirs.push(path); } continue; }
        if !metadata.is_file() { continue; }
        let allocated = metadata.blocks().saturating_mul(512); batch.scanned = batch.scanned.saturating_add(allocated);
        batch.files.push(FileFact { path, apparent: metadata.len(), allocated, device: metadata.dev(), inode: metadata.ino(),
            links: metadata.nlink(), uid: metadata.uid(), modified: metadata.mtime() });
    }
    batch
}
fn excluded(value: &str, patterns: &[String]) -> bool { patterns.iter().any(|pattern| glob_match(pattern.trim_start_matches('/'), value)) }
fn should_descend(one_file_system: bool, root_device: u64, candidate_device: u64) -> bool {
    !one_file_system || root_device == candidate_device
}
fn glob_match(pattern: &str, value: &str) -> bool {
    let (mut p, mut v, mut star, mut mark) = (0usize, 0usize, None, 0usize); let pattern = pattern.as_bytes(); let value = value.as_bytes();
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == value[v]) { p += 1; v += 1; }
        else if p < pattern.len() && pattern[p] == b'*' { star = Some(p); p += 1; mark = v; }
        else if let Some(position) = star { p = position + 1; mark += 1; v = mark; } else { return false; }
    }
    while p < pattern.len() && pattern[p] == b'*' { p += 1; } p == pattern.len()
}
#[cfg(test)] mod tests {
    use super::*;
    #[test] fn wildcard_matching() { assert!(glob_match("*.cache", "foo.cache"));
        assert!(glob_match("node_modules", "node_modules")); assert!(!glob_match("*.cache", "foo.log")); }
    #[test] fn filesystem_boundary_is_enforced_only_when_requested() {
        assert!(!should_descend(true, 1, 2)); assert!(should_descend(true, 1, 1)); assert!(should_descend(false, 1, 2));
    }
    #[test] fn missing_directory_becomes_a_scan_error() {
        let missing = PathBuf::from(format!("/tmp/qtools-missing-{}", std::process::id()));
        let batch = read_directory(missing.clone(), &missing, 0, true, &[]);
        assert_eq!(batch.errors.len(), 1);
        assert!(batch.files.is_empty());
    }
    #[test] fn exclusions_prune_directories_during_traversal() {
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("qtools-dq-scan-{}-{unique}", std::process::id()));
        fs::create_dir_all(root.join("keep")).unwrap(); fs::create_dir_all(root.join("cache")).unwrap();
        fs::write(root.join("keep/file"), b"keep").unwrap(); fs::write(root.join("cache/file"), b"skip").unwrap();
        let result = scan(&root, 2, true, &["cache".into()], false);
        assert_eq!(result.files.len(), 1);
        assert!(result.files[0].path.ends_with("keep/file"));
        assert!(!result.directories.iter().any(|directory| directory.path.ends_with("cache")));
        fs::remove_dir_all(&root).unwrap();
    }
}
