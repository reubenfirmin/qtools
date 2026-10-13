//! Concurrent directory reads and native metadata sampling.
use std::ffi::{CStr, OsStr};
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{mpsc::Sender, Arc};
use threadpool::ThreadPool;

use super::super::directory_tree::NodeId;
use super::super::progress::Progress;
use super::scheduling::WorkQueue;
use super::{DirectoryFact, FileFact};

// Small adaptive jobs return scheduling feedback sooner; two jobs per worker
// keep metadata reads outstanding while the coordinator processes results.
const ADAPTIVE_ITEMS_PER_JOB: usize = 8;
const PRECISE_ITEMS_PER_JOB: usize = 16;
const JOBS_PER_THREAD: usize = 2;
const PILOT_FILES: usize = 8;
// Complete modest leaves during their first open. Branching directories keep
// short tiny-file chunks so their newly discovered children can run promptly.
const SMALL_LEAF_FILES: usize = 32;
const SMALL_FILE_BYTES: u64 = 4096;
const BRANCH_FILE_CHUNK: usize = 32;
const LEAF_FILE_CHUNK: usize = 256;

pub(super) struct Work { pub path: PathBuf, pub node: NodeId, pub depth: usize, pub kind: WorkKind }
pub(super) enum WorkKind {
    Directory(Option<EntryMetadata>),
    Files { metadata: EntryMetadata, pending: PendingFiles, bytes_per_file: u64, has_subdirs: bool },
}

impl Work {
    pub fn directory(path: PathBuf, metadata: Option<EntryMetadata>) -> Self {
        Self { path, node: 0, depth: 0, kind: WorkKind::Directory(metadata) }
    }
    pub fn metadata(&self) -> Option<&EntryMetadata> {
        match &self.kind { WorkKind::Directory(metadata) => metadata.as_ref(), WorkKind::Files { metadata, .. } => Some(metadata) }
    }
}

pub(super) struct Batch<C> {
    pub directory: DirectoryFact, pub directory_inode: u64, pub node: NodeId,
    pub subdirs: Vec<Work>, pub files: C, pub errors: Vec<String>, pub scanned: u64,
    pub directory_visit: bool, pub continuations: Vec<Work>, pub finished_job: bool,
}
pub(super) enum WalkEvent<C> { Batch(Batch<C>), Idle }
pub(super) struct WalkOptions<'a> {
    pub threads: usize, pub one_file_system: bool, pub excludes: &'a [String], pub show_progress: bool, pub adaptive: bool,
}
#[derive(Clone)]
struct ReadOptions { root: Arc<PathBuf>, root_device: u64, one_file_system: bool, excludes: Arc<[String]>, adaptive: bool }

pub(super) trait CollectFiles: Clone + Send + 'static {
    fn push(&mut self, path: impl FnOnce() -> PathBuf, metadata: &EntryMetadata);
}

#[derive(Clone, Copy)]
pub(super) struct EntryMetadata {
    pub apparent: u64, pub allocated: u64, pub device: u64, pub inode: u64,
    pub links: u64, pub uid: u32, pub modified: i64, pub mode: u32,
}
impl From<&fs::Metadata> for EntryMetadata {
    fn from(metadata: &fs::Metadata) -> Self {
        Self { apparent: metadata.len(), allocated: metadata.blocks().saturating_mul(512), device: metadata.dev(),
            inode: metadata.ino(), links: metadata.nlink(), uid: metadata.uid(), modified: metadata.mtime(), mode: metadata.mode() }
    }
}
impl EntryMetadata {
    pub fn file_fact(&self, path: PathBuf) -> FileFact {
        FileFact { path, apparent: self.apparent, allocated: self.allocated,
            device: self.device, inode: self.inode, links: self.links, uid: self.uid, modified: self.modified }
    }
}

impl CollectFiles for Vec<FileFact> {
    fn push(&mut self, path: impl FnOnce() -> PathBuf, metadata: &EntryMetadata) { self.push(metadata.file_fact(path())); }
}

fn metadata_at(directory: &fs::File, name: &CStr) -> std::io::Result<EntryMetadata> {
    let mut item = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the descriptor and nul-terminated name remain valid. fstatat
    // initializes this buffer only on success; symlinks are never followed.
    if unsafe { libc::fstatat(directory.as_raw_fd(), name.as_ptr(), item.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let item = unsafe { item.assume_init() };
    // nlink_t is 32 bits on aarch64 and 64 bits on x86-64.
    #[allow(clippy::unnecessary_cast)]
    let links = item.st_nlink as u64;
    Ok(EntryMetadata { apparent: item.st_size as u64, allocated: (item.st_blocks as u64).saturating_mul(512),
        device: item.st_dev, inode: item.st_ino, links, uid: item.st_uid, modified: item.st_mtime, mode: item.st_mode })
}

// Packed names are transient work within this scan, never stored on disk.
#[derive(Default)]
pub(super) struct PendingFiles { bytes: Vec<u8> }
impl PendingFiles {
    fn push(&mut self, name: &CStr) {
        self.bytes.extend_from_slice(name.to_bytes_with_nul());
        self.bytes.extend_from_slice(&(name.to_bytes_with_nul().len() as u16).to_ne_bytes());
    }
    fn measure(&mut self, count: usize, mut visit: impl FnMut(&CStr)) -> u64 {
        let mut measured = 0;
        for _ in 0..count {
            if self.bytes.is_empty() { break; }
            let end = self.bytes.len() - 2;
            let size = u16::from_ne_bytes(self.bytes[end..].try_into().unwrap()) as usize;
            let start = end - size;
            visit(CStr::from_bytes_with_nul(&self.bytes[start..end]).expect("stored name has its nul terminator"));
            self.bytes.truncate(start);
            measured += 1;
        }
        measured
    }
    fn sample(&mut self, ends: &[usize], limit: usize, mut visit: impl FnMut(&CStr)) -> u64 {
        let count = ends.len();
        let samples = count.min(limit);
        let (mut next_sample, mut start, mut kept) = (0, 0, 0);
        for (index, &end) in ends.iter().enumerate() {
            let selected = if samples == 1 { 0 } else { next_sample * (count - 1) / (samples - 1) };
            if next_sample < samples && index == selected {
                visit(CStr::from_bytes_with_nul(&self.bytes[start..end - 2]).expect("stored name has its nul terminator"));
                next_sample += 1;
            } else {
                self.bytes.copy_within(start..end, kept);
                kept += end - start;
            }
            start = end;
        }
        self.bytes.truncate(kept);
        next_sample as u64
    }
}

pub(super) fn walk<C: CollectFiles>(root: &Path, options: WalkOptions<'_>, collector: C,
    mut receive: impl FnMut(WalkEvent<C>, &mut WorkQueue)) -> (Vec<String>, u128) {
    let directories_per_job = if options.adaptive { ADAPTIVE_ITEMS_PER_JOB } else { PRECISE_ITEMS_PER_JOB };
    let started = std::time::Instant::now();
    let root_device = fs::metadata(root).map(|metadata| metadata.dev()).unwrap_or(0);
    let pool = ThreadPool::new(options.threads); let (tx, rx) = std::sync::mpsc::channel();
    let root = Arc::new(root.to_path_buf());
    let read_options = ReadOptions { root: Arc::clone(&root), root_device, one_file_system: options.one_file_system,
        excludes: options.excludes.to_vec().into(), adaptive: options.adaptive };
    let mut queue = WorkQueue::new(options.adaptive);
    queue.push_ranked(Work::directory(root.as_ref().clone(), fs::metadata(root.as_path()).ok().as_ref().map(EntryMetadata::from)), 255);
    let mut in_flight = 0usize;
    let mut progress = Progress::new(options.show_progress); let mut errors = Vec::new();
    let profiling = std::env::var_os("QTOOLS_PROFILE").is_some();
    let mut dispatch_time = std::time::Duration::ZERO;
    let mut waiting_time = std::time::Duration::ZERO;
    loop {
        if queue.is_empty() && in_flight == 0 {
            // The coordinator validates the stop after all dispatched reads
            // finish. It can reopen its frontier using the same worker pool.
            receive(WalkEvent::Idle, &mut queue);
            if queue.is_empty() { break; }
        }
        let dispatch_started = profiling.then(std::time::Instant::now);
        while in_flight < options.threads.saturating_mul(JOBS_PER_THREAD) {
            let Some(directory) = queue.pop_front() else { break };
            let mut directories = vec![directory];
            for _ in 1..directories_per_job {
                let Some(directory) = queue.pop_front() else { break };
                directories.push(directory);
            }
            submit(directories, read_options.clone(), collector.clone(), &pool, tx.clone());
            in_flight += 1;
        }
        if let Some(started) = dispatch_started { dispatch_time += started.elapsed(); }
        let waiting_started = profiling.then(std::time::Instant::now);
        let batches = match rx.recv() { Ok(batches) => batches,
            Err(_) => { errors.push("scan worker channel closed unexpectedly".into()); break } };
        if let Some(started) = waiting_started { waiting_time += started.elapsed(); }
        in_flight -= 1;
        let completed = batches.iter().filter(|batch| batch.directory_visit).count();
        let scanned = batches.iter().map(|batch| batch.scanned).sum();
        let last = batches.len() - 1;
        for (index, mut batch) in batches.into_iter().enumerate() {
            batch.finished_job = index == last;
            errors.append(&mut batch.errors); receive(WalkEvent::Batch(batch), &mut queue);
        }
        progress.update(completed, scanned);
    }
    progress.finish();
    if profiling { eprintln!("dq[profile]: dispatch={}ms waiting={}ms", dispatch_time.as_millis(), waiting_time.as_millis()); }
    (errors, started.elapsed().as_millis())
}

fn submit<C: CollectFiles>(work: Vec<Work>, options: ReadOptions, collector: C, pool: &ThreadPool, tx: Sender<Vec<Batch<C>>>) {
    // Amortize pool/channel wakeups over several small directories.
    pool.execute(move || {
        let batches = work.into_iter().map(|work| read_directory_with(work, &options, collector.clone())).collect();
        let _ = tx.send(batches);
    });
}

fn read_directory_with<C: CollectFiles>(work: Work, options: &ReadOptions, files: C) -> Batch<C> {
    let metadata = work.metadata();
    let mut batch = Batch {
        directory: DirectoryFact { path: PathBuf::new(), allocated: metadata.map_or(0, |item| item.allocated), uid: metadata.map_or(0, |item| item.uid) },
        directory_inode: metadata.map_or(0, |item| item.inode), node: work.node, subdirs: Vec::new(), files, errors: Vec::new(), scanned: 0,
        directory_visit: matches!(work.kind, WorkKind::Directory(_)), continuations: Vec::new(), finished_job: false,
    };
    if options.adaptive { batch = read_directory_deferred(&work.path, work.kind, options, batch); }
    else { read_directory_precise(&work.path, options, &mut batch); }
    batch.directory.path = work.path;
    batch
}

fn read_directory_precise<C: CollectFiles>(directory: &Path, options: &ReadOptions, batch: &mut Batch<C>) {
    let entries = match fs::read_dir(directory) { Ok(entries) => entries,
        Err(error) => { batch.errors.push(format!("{}: {error}", directory.display())); return; } };
    for result in entries {
        let entry = match result { Ok(entry) => entry,
            Err(error) => { batch.errors.push(format!("{}: {error}", directory.display())); continue; } };
        if !options.excludes.is_empty() {
            let path = entry.path(); let name = entry.file_name();
            if entry_excluded(&path, &name, options) { continue; }
        }
        let file_type = match entry.file_type() { Ok(kind) => kind,
            Err(error) => { batch.errors.push(format!("{}: {error}", entry.path().display())); continue; } };
        if !file_type.is_dir() && !file_type.is_file() { continue; }
        let metadata = match entry.metadata() { Ok(metadata) => metadata,
            Err(error) => { batch.errors.push(format!("{}: {error}", entry.path().display())); continue; } };
        collect_entry(|| entry.path(), EntryMetadata::from(&metadata), options, batch);
    }
}

fn collect_entry<C: CollectFiles>(path: impl FnOnce() -> PathBuf, item: EntryMetadata, options: &ReadOptions, batch: &mut Batch<C>) {
    if !should_descend(options.one_file_system, options.root_device, item.device) { return; }
    match item.mode & libc::S_IFMT {
        libc::S_IFDIR => batch.subdirs.push(Work::directory(path(), Some(item))),
        libc::S_IFREG => {
            batch.scanned = batch.scanned.saturating_add(item.allocated);
            batch.files.push(path, &item);
        }
        _ => {}
    }
}

fn collect_native<C: CollectFiles>(directory: &Path, file: &fs::File, name: &CStr, options: &ReadOptions, batch: &mut Batch<C>) {
    let os_name = OsStr::from_bytes(name.to_bytes());
    let path = || directory.join(os_name);
    if !options.excludes.is_empty() && entry_excluded(&path(), os_name, options) { return; }
    let item = match metadata_at(file, name) { Ok(item) => item,
        Err(error) => { batch.errors.push(format!("{}: {error}", path().display())); return; } };
    collect_entry(path, item, options, batch);
}

fn read_directory_deferred<C: CollectFiles>(directory: &Path, kind: WorkKind, options: &ReadOptions, mut batch: Batch<C>) -> Batch<C> {
    use std::os::unix::fs::OpenOptionsExt;
    let flags = libc::O_DIRECTORY | if batch.node == 0 { 0 } else { libc::O_NOFOLLOW };
    let file = match fs::OpenOptions::new().read(true).custom_flags(flags).open(directory) {
        Ok(file) => file, Err(error) => { batch.errors.push(format!("{}: {error}", directory.display())); return batch; }
    };
    let metadata = match batch.directory_inode {
        0 => {
            let item = match file.metadata() { Ok(item) => item,
                Err(error) => { batch.errors.push(format!("{}: {error}", directory.display())); return batch; } };
            if !should_descend(options.one_file_system, options.root_device, item.dev()) { return batch; }
            let item = EntryMetadata::from(&item);
            batch.directory_inode = item.inode; batch.directory.allocated = item.allocated; batch.directory.uid = item.uid;
            item
        }
        _ => match &kind { WorkKind::Directory(metadata) => metadata.unwrap(), WorkKind::Files { metadata, .. } => *metadata },
    };
    let (pending, measured_files, has_subdirs) = match kind {
        WorkKind::Files { mut pending, bytes_per_file, has_subdirs, .. } => {
            // Keep branching directories responsive; leaf tails reveal only files.
            let chunk = if bytes_per_file <= SMALL_FILE_BYTES && has_subdirs { BRANCH_FILE_CHUNK } else { LEAF_FILE_CHUNK };
            let measured = pending.measure(chunk, |name| collect_native(directory, &file, name, options, &mut batch));
            (pending, measured, has_subdirs)
        }
        WorkKind::Directory(_) => {
            let (mut pending, ends) = list_directory(directory, &file, options, &mut batch);
            let has_subdirs = !batch.subdirs.is_empty();
            // Finish small leaves while their listing and inode neighborhood
            // are hot, instead of reopening them for a few deferred names.
            let limit = if !has_subdirs && ends.len() <= SMALL_LEAF_FILES { SMALL_LEAF_FILES } else { PILOT_FILES };
            let measured = pending.sample(&ends, limit, |name| collect_native(directory, &file, name, options, &mut batch));
            (pending, measured, has_subdirs)
        }
    };
    if !pending.bytes.is_empty() {
        batch.continuations.push(Work { path: directory.to_path_buf(), node: batch.node, depth: 0,
            kind: WorkKind::Files { metadata, pending, bytes_per_file: batch.scanned.checked_div(measured_files).unwrap_or(0), has_subdirs } });
    }
    batch
}

fn list_directory<C: CollectFiles>(directory: &Path, file: &fs::File, options: &ReadOptions, batch: &mut Batch<C>) -> (PendingFiles, Vec<usize>) {
    let mut pending = PendingFiles::default(); let mut ends = Vec::new();
    let mut buffer = std::mem::MaybeUninit::<[u8; 64 * 1024]>::uninit();
    loop {
        // SAFETY: getdents64 writes at most 64 KiB into this live buffer.
        // Decode its ABI as checked bytes, never unaligned struct casts.
        let count = unsafe { libc::syscall(libc::SYS_getdents64, file.as_raw_fd(), buffer.as_mut_ptr(), 64 * 1024_usize) };
        if count < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted { continue; }
            batch.errors.push(format!("{}: {error}", directory.display())); break;
        }
        if count == 0 { break; }
        // SAFETY: only the bytes initialized by the syscall are read.
        let entries = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), count as usize) };
        let mut offset = 0;
        while offset < entries.len() {
            let record = &entries[offset..];
            let size = record.get(16..18).map(|bytes| u16::from_ne_bytes(bytes.try_into().unwrap()) as usize);
            let Some(size) = size.filter(|size| *size >= 20 && *size <= record.len()) else {
                batch.errors.push(format!("{}: invalid directory entry", directory.display())); break;
            };
            let kind = record[18];
            let name = match CStr::from_bytes_until_nul(&record[19..size]) { Ok(name) => name,
                Err(_) => { batch.errors.push(format!("{}: unterminated directory entry", directory.display())); break; } };
            offset += size;
            if name.to_bytes() == b"." || name.to_bytes() == b".." { continue; }
            if kind != libc::DT_DIR && kind != libc::DT_REG && kind != libc::DT_UNKNOWN { continue; }
            if !options.excludes.is_empty() && entry_excluded(&directory.join(OsStr::from_bytes(name.to_bytes())), OsStr::from_bytes(name.to_bytes()), options) { continue; }
            match kind {
                // Opening the child provides its metadata and device.
                libc::DT_DIR => batch.subdirs.push(Work::directory(directory.join(OsStr::from_bytes(name.to_bytes())), None)),
                libc::DT_REG => { pending.push(name); ends.push(pending.bytes.len()); }
                _ => collect_native(directory, file, name, options, batch),
            }
        }
    }
    (pending, ends)
}

fn entry_excluded(path: &Path, name: &OsStr, options: &ReadOptions) -> bool {
    let relative = path.strip_prefix(options.root.as_path()).unwrap_or(path).to_string_lossy();
    excluded(&relative, &options.excludes) || excluded(&name.to_string_lossy(), &options.excludes)
}
fn excluded(value: &str, patterns: &[String]) -> bool { patterns.iter().any(|pattern| glob_match(pattern.trim_start_matches('/'), value)) }
pub(super) fn should_descend(one_file_system: bool, root_device: u64, candidate_device: u64) -> bool {
    !one_file_system || root_device == candidate_device
}
pub(super) fn glob_match(pattern: &str, value: &str) -> bool {
    let (mut p, mut v, mut star, mut mark) = (0usize, 0usize, None, 0usize); let pattern = pattern.as_bytes(); let value = value.as_bytes();
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == value[v]) { p += 1; v += 1; }
        else if p < pattern.len() && pattern[p] == b'*' { star = Some(p); p += 1; mark = v; }
        else if let Some(position) = star { p = position + 1; mark += 1; v = mark; } else { return false; }
    }
    while p < pattern.len() && pattern[p] == b'*' { p += 1; } p == pattern.len()
}

#[cfg(test)]
pub(super) fn read_directory(directory: PathBuf, root: &Path, root_device: u64, one_file_system: bool, excludes: &[String]) -> Batch<Vec<FileFact>> {
    let metadata = fs::symlink_metadata(&directory).ok().as_ref().map(EntryMetadata::from);
    let options = ReadOptions { root: Arc::new(root.to_path_buf()), root_device, one_file_system, excludes: excludes.to_vec().into(), adaptive: false };
    read_directory_with(Work::directory(directory, metadata), &options, Vec::new())
}
