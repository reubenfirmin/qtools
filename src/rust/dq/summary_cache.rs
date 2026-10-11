//! Past measurements guide access order; cached bytes never enter totals.
use std::fs::{self, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use serde::{Deserialize, Serialize};
use super::disk_scan::SummaryScan;

const VERSION: u32 = 21;
const MAX_BYTES: u64 = 256 * 1024 * 1024;
const MAX_WORDS: usize = 1 << 20;
pub(super) const FILE_HINT_MIN: u64 = 4096;
pub(super) const MAX_FILE_HINTS: usize = 4_000_000;

#[derive(Clone, PartialEq)]
pub(super) struct FileHints { pub directory: PathBuf, pub names: Vec<u8>, pub prior_bytes: u64 }

pub(super) fn names(mut packed: &[u8]) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        if packed.is_empty() { return None; }
        let end = packed.len().checked_sub(2)?;
        let size = u16::from_ne_bytes(packed[end..].try_into().ok()?) as usize;
        let start = end.checked_sub(size)?;
        let name = packed.get(start..end)?.strip_suffix(&[0])?;
        packed = &packed[..start];
        Some(name)
    })
}

#[derive(Clone, Default)]
pub(super) struct SmallFiles { words: Vec<u64>, directory_weights: HashMap<u32, u8> }
impl SmallFiles {
    pub fn contains(&self, inode: u64) -> bool {
        self.words.get((inode / 64) as usize).is_some_and(|word| word & (1 << (inode % 64)) != 0)
    }
    pub fn is_empty(&self) -> bool { self.words.is_empty() }
    pub fn directory_bytes(&self, inode: u64) -> u64 {
        u32::try_from(inode).ok().and_then(|inode| self.directory_weights.get(&inode)).map(|weight| weight & 0x7f).filter(|weight| *weight > 0)
            .map_or(0, |weight| 1_u64 << (weight - 1))
    }
    pub fn directory_small(&self, inode: u64) -> bool {
        u32::try_from(inode).ok().and_then(|inode| self.directory_weights.get(&inode)).is_some_and(|weight| weight & 0x80 != 0)
    }
    pub fn remember_directory(&mut self, inode: u64, bytes: u64, small: Option<bool>, partial: bool) {
        let Ok(inode) = u32::try_from(inode) else { return; };
        if !self.directory_weights.contains_key(&inode) && self.directory_weights.len() >= MAX_WORDS { return; }
        let weight = bytes.max(1).ilog2() as u8 + 1;
        let old = self.directory_weights.entry(inode).or_default();
        let flag = small.map_or(*old & 0x80, |small| if small { 0x80 } else { 0 });
        *old = (if partial { (*old & 0x7f).max(weight) } else { weight }) | flag;
    }
    pub fn carry_directory_weights(&mut self, initial: &Self) {
        self.directory_weights.clone_from(&initial.directory_weights);
    }
    #[cfg(test)]
    pub fn update(&mut self, inode: u64, small: bool) {
        let index = (inode / 64) as usize;
        if index >= MAX_WORDS || (!small && index >= self.words.len()) { return; }
        if self.words.len() <= index { self.words.resize(index + 1, 0); }
        let bit = 1 << (inode % 64);
        if small { self.words[index] |= bit; } else { self.words[index] &= !bit; }
    }
}

// Workers update ordering hints directly, avoiding a per-file vector and a
// serialized pass on the coordinator. These atomics never carry byte totals.
pub(super) struct SmallUpdates { words: Vec<AtomicU64> }
impl SmallUpdates {
    pub fn new(initial: &SmallFiles) -> Self {
        let words = (0..MAX_WORDS).map(|index| AtomicU64::new(initial.words.get(index).copied().unwrap_or(0))).collect();
        Self { words }
    }
    pub fn update(&self, inode: u64, small: bool) {
        let Some(word) = self.words.get((inode / 64) as usize) else { return; };
        let bit = 1 << (inode % 64);
        if (word.load(Ordering::Relaxed) & bit != 0) == small { return; }
        if small { word.fetch_or(bit, Ordering::Relaxed); } else { word.fetch_and(!bit, Ordering::Relaxed); }
    }
    pub fn snapshot(&self) -> SmallFiles {
        let mut words = self.words.iter().map(|word| word.load(Ordering::Relaxed)).collect::<Vec<_>>();
        while words.last() == Some(&0) { words.pop(); }
        SmallFiles { words, directory_weights: HashMap::new() }
    }
}

#[derive(Serialize, Deserialize, PartialEq)]
struct Key { version: u32, root: PathBuf, canonical: PathBuf, device: u64, inode: u64 }
#[derive(Serialize, Deserialize)]
struct Saved { key: Key, hints: Vec<(PathBuf, u64)> }
pub(super) struct Cache { key: Key, file: PathBuf, hints: Vec<(PathBuf, u64)>, candidates: Option<Candidates>, small: Arc<SmallFiles> }
struct Loaded { hints: Vec<(PathBuf, u64)>, candidates: Candidates, small: SmallFiles }

pub(super) struct Candidates { reader: BufReader<fs::File>, remaining: usize, files: usize }
impl Candidates {
    #[cfg(test)]
    pub fn monitor_reads(&self) -> std::io::Result<fs::File> { self.reader.get_ref().try_clone() }

    pub fn next(&mut self) -> Option<FileHints> {
        if self.remaining == 0 { return None; }
        self.remaining -= 1;
        let mut header = [0; 16]; self.reader.read_exact(&mut header).ok()?;
        let path_size = u32::from_le_bytes(header[..4].try_into().ok()?) as usize;
        let names_size = u32::from_le_bytes(header[4..8].try_into().ok()?) as usize;
        let prior_bytes = u64::from_le_bytes(header[8..16].try_into().ok()?);
        if path_size > 1024 * 1024 || names_size as u64 > MAX_BYTES { return None; }
        let mut path = vec![0; path_size]; self.reader.read_exact(&mut path).ok()?;
        if path.contains(&0) { return None; }
        let directory = PathBuf::from(OsString::from_vec(path));
        if !directory.components().all(|part| matches!(part, std::path::Component::Normal(_))) { return None; }
        let mut packed = vec![0; names_size]; self.reader.read_exact(&mut packed).ok()?;
        let mut checked_size = 0;
        for name in names(&packed) {
            if name.is_empty() || name == b"." || name == b".." || name.len() > 255 || name.contains(&b'/') || name.contains(&0) { return None; }
            checked_size += name.len() + 3; self.files += 1;
        }
        if checked_size != packed.len() || self.files > MAX_FILE_HINTS { return None; }
        Some(FileHints { directory, names: packed, prior_bytes })
    }
}

impl Cache {
    pub fn open(root: &Path) -> Option<Self> {
        if std::env::var_os("QTOOLS_NO_CACHE").is_some() { return None; }
        let canonical = fs::canonicalize(root).ok()?;
        let metadata = fs::metadata(&canonical).ok()?;
        let key = Key { version: VERSION, root: canonical.clone(), canonical, device: metadata.dev(), inode: metadata.ino() };
        let uid = unsafe { libc::geteuid() };
        let directory = std::env::temp_dir().join(format!("qtools-dq-cache-{uid}"));
        let _ = fs::DirBuilder::new().mode(0o700).create(&directory);
        let metadata = fs::symlink_metadata(&directory).ok()?;
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 { return None; }
        let encoded = serde_json::to_vec(&key).ok()?;
        let hash = encoded.iter().fold(14_695_981_039_346_656_037_u64,
            |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(1_099_511_628_211));
        let file = directory.join(format!("hints-v{VERSION}-{hash:016x}.json"));
        let mut cache = Self { key, file, hints: Vec::new(), candidates: None, small: Arc::new(SmallFiles::default()) };
        if let Some(loaded) = cache.read_hints() {
            cache.hints = loaded.hints; cache.candidates = Some(loaded.candidates); cache.small = Arc::new(loaded.small);
        }
        Some(cache)
    }

    fn read_hints(&self) -> Option<Loaded> {
        let mut file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(&self.file).ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0 || metadata.len() > MAX_BYTES { return None; }
        let mut header = [0; 16]; file.read_exact(&mut header).ok()?;
        if &header[..4] != b"DQHA" { return None; }
        let size = u32::from_le_bytes(header[4..8].try_into().ok()?) as usize;
        let words = u32::from_le_bytes(header[8..12].try_into().ok()?) as usize;
        let directories = u32::from_le_bytes(header[12..16].try_into().ok()?) as usize;
        if size as u64 > MAX_BYTES || words > MAX_WORDS || directories > MAX_WORDS
            || metadata.len() < 20 + size as u64 + words as u64 * 8 + directories as u64 * 5 { return None; }
        let mut encoded = vec![0; size]; file.read_exact(&mut encoded).ok()?;
        let saved: Saved = serde_json::from_slice(&encoded).ok()?;
        if saved.key != self.key { return None; }
        let mut encoded = vec![0; words * 8]; file.read_exact(&mut encoded).ok()?;
        let words = encoded.as_chunks::<8>().0.iter().map(|word| u64::from_le_bytes(*word)).collect();
        let mut small = SmallFiles { words, directory_weights: HashMap::with_capacity(directories) };
        let mut encoded = vec![0; directories * 5]; file.read_exact(&mut encoded).ok()?;
        for entry in encoded.as_chunks::<5>().0 {
            let inode = u32::from_le_bytes(entry[..4].try_into().ok()?);
            let weight = entry[4];
            if weight & 0x7f == 0 || weight & 0x7f > 64 { return None; }
            small.directory_weights.insert(inode, weight);
        }
        let mut count = [0; 4]; file.read_exact(&mut count).ok()?;
        let count = u32::from_le_bytes(count) as usize;
        if count > MAX_WORDS { return None; }
        Some(Loaded { hints: saved.hints, candidates: Candidates { reader: BufReader::new(file), remaining: count, files: 0 }, small })
    }

    pub fn hints(&self) -> &[(PathBuf, u64)] { &self.hints }
    pub fn take_candidates(&mut self) -> Option<Candidates> { self.candidates.take().filter(|index| index.remaining > 0) }
    pub fn small_files(&self) -> Arc<SmallFiles> { Arc::clone(&self.small) }

    pub fn save(self, scan: &SummaryScan) {
        // Retain the complete index after a partial query. Rewriting it from
        // just the visited prefix would destroy the advantage of lazy access.
        if scan.pruned_directories > 0 || scan.file_hints.len() > MAX_WORDS { return; }
        if self.hints == scan.hints && scan.file_hints.is_empty() && self.small.words == scan.small_files.words
            && self.small.directory_weights == scan.small_files.directory_weights { return; }
        let saved = SavedRef { key: &self.key, hints: &scan.hints };
        let Ok(encoded) = serde_json::to_vec(&saved) else { return; };
        let size = 20 + encoded.len() as u64 + scan.small_files.words.len() as u64 * 8
            + scan.small_files.directory_weights.len() as u64 * 5
            + scan.file_hints.iter().map(|group| 16 + group.directory.as_os_str().len() as u64 + group.names.len() as u64).sum::<u64>();
        if size > MAX_BYTES { return; }
        let temporary = self.file.with_extension(format!("{}.tmp", std::process::id()));
        let result = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
            let directories = scan.small_files.directory_weights.iter()
                .map(|(inode, weight)| { let mut entry = [0; 5]; entry[..4].copy_from_slice(&inode.to_le_bytes()); entry[4] = *weight; entry }).collect::<Vec<_>>();
            if directories.len() > MAX_WORDS { return Ok(()); }
            file.write_all(b"DQHA")?;
            file.write_all(&(encoded.len() as u32).to_le_bytes())?;
            file.write_all(&(scan.small_files.words.len() as u32).to_le_bytes())?;
            file.write_all(&(directories.len() as u32).to_le_bytes())?;
            file.write_all(&encoded)?;
            let encoded = scan.small_files.words.iter().flat_map(|word| word.to_le_bytes()).collect::<Vec<_>>();
            file.write_all(&encoded)?;
            for chunk in directories.chunks(8192) { file.write_all(chunk.as_flattened())?; }
            file.write_all(&(scan.file_hints.len() as u32).to_le_bytes())?;
            let mut packed = Vec::with_capacity((size - 20 - encoded.len() as u64) as usize);
            for group in &scan.file_hints {
                let path = group.directory.as_os_str().as_bytes();
                packed.extend_from_slice(&(path.len() as u32).to_le_bytes());
                packed.extend_from_slice(&(group.names.len() as u32).to_le_bytes());
                packed.extend_from_slice(&group.prior_bytes.to_le_bytes());
                packed.extend_from_slice(path); packed.extend_from_slice(&group.names);
            }
            file.write_all(&packed)?;
            fs::rename(&temporary, &self.file)
        })();
        if result.is_err() { let _ = fs::remove_file(&temporary); }
    }
}

#[derive(Serialize)]
struct SavedRef<'a> { key: &'a Key, hints: &'a [(PathBuf, u64)] }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_hints_roundtrip_and_corrupt_files_fall_back_to_traversal() {
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("qtools-dq-cache-test-{}-{unique}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let cache = Cache::open(&root).unwrap();
        let file = cache.file.clone();
        let mut small = SmallFiles::default();
        small.update(42, true); small.update(100_000, true); small.update(42, false);
        small.update(u64::MAX, true);
        small.remember_directory(500, 1_000_000, Some(false), false);
        small.remember_directory(500, 32, None, true);
        small.remember_directory(600, 1_000, Some(true), false);
        assert!(!small.contains(42)); assert!(!small.contains(u64::MAX));
        let scan = SummaryScan { root: root.clone(), levels: Vec::new(), scanned_files: 0, selected_files: 0,
            directories: 0, duplicate_hard_links: 0, errors: Vec::new(), duration_ms: 0, pruned_directories: 0,
            unmeasured_bytes_upper_bound: None, known_unassigned_bytes: 0,
            hints: vec![(PathBuf::from("data/large"), 1_000_000)],
            file_hints: vec![FileHints { directory: PathBuf::from("data/large"), names: b"big\0\x04\x00".to_vec(), prior_bytes: 1_000_000 },
                FileHints { directory: PathBuf::from("data/large"), names: b"tiny\0\x05\x00".to_vec(), prior_bytes: 4096 }], small_files: small };
        cache.save(&scan);
        let alias = Cache::open(&root.join(".")).unwrap();
        assert_eq!(alias.file, file, "equivalent root paths must share the learned index");
        assert_eq!(alias.hints, scan.hints);
        assert!(alias.candidates.is_some());
        let mut loaded = Cache::open(&root).unwrap();
        assert_eq!(loaded.hints, scan.hints);
        assert!(loaded.small.contains(100_000)); assert!(!loaded.small.contains(42));
        assert_eq!(loaded.small.directory_bytes(500), 524_288);
        assert!(!loaded.small.directory_small(500)); assert!(loaded.small.directory_small(600));
        let mut candidates = loaded.take_candidates().unwrap();
        for expected in &scan.file_hints { assert!(candidates.next().as_ref() == Some(expected)); }
        assert!(candidates.next().is_none());
        fs::write(&file, b"DQHA\xff\xff\xff\xff\xff\xff\xff\xff").unwrap();
        let fallback = Cache::open(&root).unwrap();
        assert!(fallback.hints.is_empty()); assert!(fallback.small.is_empty());
        fs::remove_file(file).unwrap(); fs::remove_dir(root).unwrap();
    }
}
