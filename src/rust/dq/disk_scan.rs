//! Concurrent filesystem traversal with detailed facts or streaming directory totals.
use std::fs;
use std::ffi::{CStr, CString, OsStr};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::collections::{HashMap, HashSet, VecDeque};
use threadpool::ThreadPool;
use crate::dq::progress::Progress;
use super::dq_query::{DiskQuery, SizeAccounting};
use super::directory_tree::{DirectoryTree, NodeId};
use super::hierarchy::Totals;
use super::pruning::{self, AllocationChecks, Bound};
use super::summary_cache::{Candidates, FileHints, SmallFiles, SmallUpdates};

#[derive(Debug, Clone)] pub struct FileFact { pub path: PathBuf, pub apparent: u64, pub allocated: u64, pub device: u64,
    pub inode: u64, pub links: u64, pub uid: u32, pub modified: i64 }
#[derive(Debug, Clone)] pub struct DirectoryFact { pub path: PathBuf, pub allocated: u64, pub uid: u32 }
#[derive(Debug)] pub struct Scan { pub root: PathBuf, pub files: Vec<FileFact>, pub directories: Vec<DirectoryFact>,
    pub errors: Vec<String>, pub duration_ms: u128 }
pub(super) struct SummaryScan {
    pub root: PathBuf, pub levels: Vec<Totals>, pub scanned_files: usize, pub selected_files: usize,
    pub directories: usize, pub duplicate_hard_links: usize, pub errors: Vec<String>, pub duration_ms: u128,
    pub pruned_directories: usize, pub unmeasured_bytes_upper_bound: Option<u64>,
    pub known_unassigned_bytes: u64,
    pub hints: Vec<(PathBuf, u64)>,
    pub file_hints: Vec<FileHints>,
    pub small_files: SmallFiles,
}

#[derive(Clone)]
struct Work { path: PathBuf, metadata: Option<EntryMetadata>, node: NodeId, prior_bytes: u64,
    pending: Option<Vec<u8>>, small_branch: bool, probe: bool, registered: bool, descriptor: Option<Arc<fs::File>> }
struct Batch<C> { directory: DirectoryFact, directory_inode: u64, node: NodeId, subdirs: Vec<Work>, files: C,
    errors: Vec<String>, scanned: u64, directory_visit: bool, probe: bool, continuations: Vec<Work>, finished_job: bool,
    units: usize, new_batches: usize, measured_directories: Vec<(u64, u64)> }
struct WalkOptions<'a> { threads: usize, one_file_system: bool, excludes: &'a [String], show_progress: bool, adaptive: bool,
    hints: &'a [(PathBuf, u64)], small: Arc<SmallFiles>, index: Option<Arc<SmallUpdates>>, candidates: Option<Candidates>, root_directory: Option<Arc<fs::File>> }
#[derive(Clone)]
struct ReadOptions { root: Arc<PathBuf>, root_device: u64, one_file_system: bool, excludes: Arc<[String]>, hints: Arc<HashMap<PathBuf, u64>>,
    small: Arc<SmallFiles>, index: Option<Arc<SmallUpdates>>, adaptive: bool, root_directory: Option<Arc<fs::File>>, streaming: bool }

type Priority = u8;

// The priority is an ordering hint, not an allocation bound. Logarithmic
// buckets preserve useful contrasts while keeping push/pop independent of
// the number of pending directories. A bitmap selects the highest bucket.
struct PriorityBuckets {
    buckets: [VecDeque<Work>; 256], occupied: [u64; 4], len: usize,
}
impl PriorityBuckets {
    fn new() -> Self { Self { buckets: std::array::from_fn(|_| VecDeque::new()), occupied: [0; 4], len: 0 } }
    fn push(&mut self, work: Work, priority: Priority) {
        let index = usize::from(priority);
        self.buckets[index].push_back(work);
        self.occupied[index / 64] |= 1 << (index % 64);
        self.len += 1;
    }
    fn push_preserving_order(&mut self, work: Work, priority: Priority) {
        let index = usize::from(priority);
        self.buckets[index].push_front(work);
        self.occupied[index / 64] |= 1 << (index % 64);
        self.len += 1;
    }
    fn pop(&mut self) -> Option<Work> {
        for word in (0..4).rev() {
            if self.occupied[word] == 0 { continue; }
            let bit = 63 - self.occupied[word].leading_zeros() as usize;
            let index = word * 64 + bit;
            let work = self.buckets[index].pop_back();
            self.len -= 1;
            if self.buckets[index].is_empty() { self.occupied[word] &= !(1 << bit); }
            return work;
        }
        None
    }
}

struct WorkQueue { fifo: VecDeque<Work>, buckets: Option<Box<PriorityBuckets>>, candidates: Option<Candidates>,
    candidate_root: PathBuf }
impl WorkQueue {
    fn new(adaptive: bool) -> Self { Self { fifo: VecDeque::new(), buckets: adaptive.then(|| Box::new(PriorityBuckets::new())),
        candidates: None, candidate_root: PathBuf::new() } }
    fn len(&self) -> usize { self.buckets.as_ref().map_or(self.fifo.len(), |buckets| buckets.len) }
    fn is_empty(&self) -> bool { self.len() == 0 && self.candidates.is_none() }
    fn pop_front(&mut self) -> Option<Work> {
        while self.len() < 1024 && self.candidates.is_some() {
            let Some(group) = self.candidates.as_mut().unwrap().next() else { self.candidates = None; break; };
            let work = Work { path: self.candidate_root.join(group.directory), metadata: None, node: usize::MAX,
                prior_bytes: group.prior_bytes, pending: Some(group.names), small_branch: false, probe: true, registered: false, descriptor: None };
            self.buckets.as_mut().unwrap().push_preserving_order(work, 254);
        }
        match &mut self.buckets { Some(buckets) => buckets.pop(), None => self.fifo.pop_front() }
    }
    fn push_ranked(&mut self, work: Work, priority: Priority) {
        if let Some(buckets) = &mut self.buckets { buckets.push(work, priority); }
        else { self.fifo.push_back(work); }
    }
    fn extend(&mut self, work: impl IntoIterator<Item = Work>) {
        for work in work { self.push_ranked(work, 0); }
    }
    fn update(&mut self, mut update: impl FnMut(Work) -> Option<(Work, Priority)>) {
        if let Some(mut old) = self.buckets.take() {
            self.buckets = Some(Box::new(PriorityBuckets::new()));
            while let Some(work) = old.pop() {
                if let Some((work, priority)) = update(work) {
                    self.buckets.as_mut().unwrap().push_preserving_order(work, priority);
                }
            }
        } else {
            let old = std::mem::take(&mut self.fifo);
            for work in old {
                if let Some((work, priority)) = update(work) { self.push_ranked(work, priority); }
            }
        }
    }
}

trait CollectFiles: Clone + Send + 'static {
    fn push(&mut self, path: impl FnOnce() -> PathBuf, metadata: &EntryMetadata);
    fn remember(&mut self, _name: &OsStr, _allocated: u64) {}
    fn merge(&mut self, other: Self);
}

#[derive(Clone, Copy)]
struct EntryMetadata { apparent: u64, allocated: u64, device: u64, inode: u64, links: u64, uid: u32, modified: i64, mode: u32 }
impl From<&fs::Metadata> for EntryMetadata {
    fn from(metadata: &fs::Metadata) -> Self {
        Self { apparent: metadata.len(), allocated: metadata.blocks().saturating_mul(512), device: metadata.dev(),
            inode: metadata.ino(), links: metadata.nlink(), uid: metadata.uid(), modified: metadata.mtime(), mode: metadata.mode() }
    }
}
fn metadata_at(directory: &fs::File, name: &CStr) -> std::io::Result<EntryMetadata> {
    let mut item = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the descriptor and nul-terminated name remain valid. fstatat
    // initializes this buffer only on success; symlinks are never followed.
    if unsafe { libc::fstatat(directory.as_raw_fd(), name.as_ptr(), item.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let item = unsafe { item.assume_init() };
    Ok(EntryMetadata { apparent: item.st_size as u64, allocated: (item.st_blocks as u64).saturating_mul(512),
        device: item.st_dev, inode: item.st_ino, links: item.st_nlink, uid: item.st_uid, modified: item.st_mtime, mode: item.st_mode })
}

fn secure_root(root: &Path) -> Option<Arc<fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY).open(root).ok()?;
    // Probe the kernel capability once. Unsupported kernels use traversal.
    let relative = c".";
    let how = [libc::O_RDONLY as u64 | libc::O_DIRECTORY as u64 | libc::O_CLOEXEC as u64, 0, 0x08 | 0x04];
    // SAFETY: the descriptor, C string, and three-u64 open_how buffer are
    // live for the syscall. Every successful descriptor is immediately owned.
    let descriptor = unsafe { libc::syscall(libc::SYS_openat2, file.as_raw_fd(), relative.as_ptr(), how.as_ptr(), std::mem::size_of_val(&how)) };
    if descriptor < 0 { return None; }
    drop(unsafe { fs::File::from_raw_fd(descriptor as i32) });
    Some(Arc::new(file))
}

fn open_probe(directory: &Path, options: &ReadOptions) -> std::io::Result<fs::File> {
    let root = options.root_directory.as_ref().expect("probes require a checked root descriptor");
    let relative = directory.strip_prefix(options.root.as_path()).expect("probe stays below the root");
    let relative = if relative.as_os_str().is_empty() { c".".to_owned() }
        else { CString::new(relative.as_os_str().as_bytes()).expect("validated probe path") };
    let how = [libc::O_RDONLY as u64 | libc::O_DIRECTORY as u64 | libc::O_CLOEXEC as u64, 0, 0x08 | 0x04];
    // BENEATH | NO_SYMLINKS prevents stale hints following a replaced ancestor
    // outside the root. The fresh directory device is checked after opening.
    let descriptor = unsafe { libc::syscall(libc::SYS_openat2, root.as_raw_fd(), relative.as_ptr(), how.as_ptr(), std::mem::size_of_val(&how)) };
    if descriptor < 0 { return Err(std::io::Error::last_os_error()); }
    Ok(unsafe { fs::File::from_raw_fd(descriptor as i32) })
}
fn file_fact(path: PathBuf, metadata: &EntryMetadata) -> FileFact {
    FileFact { path, apparent: metadata.apparent, allocated: metadata.allocated,
        device: metadata.device, inode: metadata.inode, links: metadata.links, uid: metadata.uid, modified: metadata.modified }
}

impl CollectFiles for Vec<FileFact> {
    fn push(&mut self, path: impl FnOnce() -> PathBuf, metadata: &EntryMetadata) { self.push(file_fact(path(), metadata)); }
    fn merge(&mut self, other: Self) { self.extend(other); }
}

#[derive(Clone, Copy)]
struct FileSelection { accounting: SizeAccounting, min_size: Option<u64>, older_than: Option<u64>, now: i64 }

impl FileSelection {
    fn matches(&self, bytes: u64, modified: i64) -> bool {
        !self.min_size.is_some_and(|minimum| bytes < minimum)
            && !self.older_than.is_some_and(|age| self.now.saturating_sub(modified) < age as i64)
    }
}

#[derive(Clone)]
struct SummaryFiles {
    selection: FileSelection, bytes: u64, selected: u64, scanned: usize, hard_links: Vec<FileFact>,
    bound_device: Option<u64>, check_file_inodes: bool, bound_bytes: u64, allocations: Vec<(u64, u64)>,
    allocation_checks: Option<Arc<AllocationChecks>>,
    has_large_files: bool,
    remember_files: bool, big_names: Vec<u8>, big_bytes: u64,
}

struct HardLinkGroup {
    fact: FileFact, directory: NodeId, observed: u64, finalized: bool,
    // The representative already identifies the first link. Larger unfinished
    // indexed groups retain distinct addresses to detect a corrupt duplicate.
    indexed_paths: Option<HashSet<PathBuf>>,
}

impl CollectFiles for SummaryFiles {
    fn merge(&mut self, mut other: Self) {
        self.bytes += other.bytes; self.selected += other.selected; self.scanned += other.scanned;
        self.bound_bytes += other.bound_bytes; self.has_large_files |= other.has_large_files;
        self.hard_links.append(&mut other.hard_links); self.allocations.append(&mut other.allocations);
        self.big_names.append(&mut other.big_names); self.big_bytes += other.big_bytes;
    }
    fn push(&mut self, path: impl FnOnce() -> PathBuf, metadata: &EntryMetadata) {
        self.scanned += 1;
        let allocated = metadata.allocated;
        self.has_large_files |= allocated > 4096;
        // Only multiply-linked inodes need paths and deterministic representatives.
        if metadata.links > 1 {
            let path = path();
            self.hard_links.push(file_fact(path, metadata)); return;
        }
        if self.bound_device == Some(metadata.device) {
            if let Some(checks) = &self.allocation_checks { checks.record(metadata.inode); }
            if self.check_file_inodes { self.allocations.push((metadata.inode, allocated)); }
            else { self.bound_bytes += allocated; }
        }
        let bytes = match self.selection.accounting {
            SizeAccounting::Allocated => allocated, SizeAccounting::Apparent => metadata.apparent,
        };
        if self.selection.matches(bytes, metadata.modified) { self.bytes += bytes; self.selected += 1; }
    }
    fn remember(&mut self, name: &OsStr, allocated: u64) {
        if self.remember_files && allocated >= super::summary_cache::FILE_HINT_MIN && name.len() <= 255 {
            self.big_names.extend_from_slice(name.as_bytes()); self.big_names.push(0);
            self.big_names.extend_from_slice(&((name.len() + 1) as u16).to_ne_bytes());
            self.big_bytes = self.big_bytes.saturating_add(allocated);
        }
    }
}

pub fn scan(root: &Path, threads: usize, one_file_system: bool, excludes: &[String], show_progress: bool) -> Scan {
    let mut files = Vec::new(); let mut directories = Vec::new();
    let (errors, duration_ms) = walk(root, WalkOptions { threads, one_file_system, excludes, show_progress, adaptive: false, hints: &[], small: Arc::default(), index: None, candidates: None, root_directory: None },
        Vec::<FileFact>::new(), |batch, queue| {
            queue.extend(batch.subdirs); directories.push(batch.directory); files.extend(batch.files);
        });
    Scan { root: root.to_path_buf(), files, directories, errors, duration_ms }
}

pub(super) fn summarize(root: &Path, query: &DiskQuery) -> SummaryScan {
    let started = std::time::Instant::now();
    let mut bound = Bound::for_query(root, query);
    let mut cache = bound.as_ref().and_then(|_| super::summary_cache::Cache::open(root));
    let candidates = cache.as_mut().and_then(|cache| cache.take_candidates());
    let hints = cache.as_ref().map_or(&[][..], |cache| cache.hints());
    let small = cache.as_ref().map_or_else(Arc::default, |cache| cache.small_files());
    // A partial index can contain only just enough bytes to meet the bound,
    // forcing every indexed candidate to be revisited. Establish a complete
    // address index once; subsequent scans can select a much smaller prefix.
    if cache.is_some() && candidates.is_none() { bound = None; }
    let setup = started.elapsed();
    let mut scan = summarize_with_sources(root, query, bound, hints, small, cache.is_some(), candidates);
    let walked = started.elapsed();
    if let Some(cache) = cache { cache.save(&scan); }
    if std::env::var_os("QTOOLS_PROFILE").is_some() {
        eprintln!("dq[profile]: hint-load={}ms scan={}ms hint-save={}ms", setup.as_millis(),
            walked.saturating_sub(setup).as_millis(), started.elapsed().saturating_sub(walked).as_millis());
    }
    scan.duration_ms = started.elapsed().as_millis();
    scan
}

#[cfg(test)]
fn summarize_with_bound(root: &Path, query: &DiskQuery, mut bound: Option<Bound>) -> SummaryScan {
    summarize_with_hints(root, query, bound.take(), &[], Arc::default(), false)
}

#[cfg(test)]
fn summarize_with_hints(root: &Path, query: &DiskQuery, bound: Option<Bound>,
    hints: &[(PathBuf, u64)], small: Arc<SmallFiles>, remember: bool) -> SummaryScan {
    summarize_with_sources(root, query, bound, hints, small, remember, None)
}

fn summarize_with_sources(root: &Path, query: &DiskQuery, mut bound: Option<Bound>,
    hints: &[(PathBuf, u64)], small: Arc<SmallFiles>, remember: bool, mut candidates: Option<Candidates>) -> SummaryScan {
    let started = std::time::Instant::now();
    let profiling = std::env::var_os("QTOOLS_PROFILE").is_some();
    let mut accounting_time = std::time::Duration::ZERO;
    let mut scheduling_time = std::time::Duration::ZERO;
    let mut batches_seen = 0;
    let mut continuation_batches = 0;
    let mut directory_hints = std::collections::BinaryHeap::new();
    let depths = if query.presentation.depth == 1 { vec![1, 2, 3] } else { vec![query.presentation.depth] };
    let selection = FileSelection { accounting: query.accounting, min_size: query.selection.min_size,
        older_than: query.selection.older_than.map(|age| age.as_secs()),
        now: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64 };
    let bound_device = bound.as_ref().and_then(|_| fs::metadata(root).ok().map(|item| item.dev()));
    let mut collector = SummaryFiles { selection, bytes: 0, selected: 0, scanned: 0, hard_links: Vec::new(),
        bound_device, check_file_inodes: bound.as_ref().is_some_and(Bound::check_file_inodes), bound_bytes: 0, allocations: Vec::new(), allocation_checks: None,
        has_large_files: false, remember_files: remember, big_names: Vec::new(), big_bytes: 0 };
    let initial_small = Arc::clone(&small);
    let report_depth = *depths.iter().max().unwrap();
    let mut tree = DirectoryTree::new(report_depth);
    let root_directory = bound.as_ref().filter(|bound| !bound.check_file_inodes())
        .filter(|_| candidates.is_some()).and_then(|_| secure_root(root));
    if root_directory.is_none() { candidates = None; }
    let indexed = candidates.is_some();
    // Partial indexed scans retain the existing complete index. Only ordinary
    // traversal needs to collect hints; an indexed fallback rebuilds them.
    let collect_hints = remember && !indexed;
    collector.remember_files = collect_hints;
    let index = (!indexed && (remember || !small.is_empty()))
        .then(|| Arc::new(SmallUpdates::new(small.as_ref())));
    let allocation_checks = indexed.then(|| Arc::new(AllocationChecks::new()));
    if indexed {
        collector.allocation_checks = allocation_checks.clone();
    }
    let mut measured_directory_metadata = HashSet::new();
    let options = WalkOptions { threads: query.traversal.threads, one_file_system: query.traversal.one_file_system,
        excludes: &query.selection.excludes, show_progress: query.traversal.show_progress && !query.output.machine(), adaptive: bound.is_some(), hints, small, index: index.clone(), candidates, root_directory };
    let mut scanned_files = 0; let mut selected_files = 0; let mut directories = 0; let mut duplicate_hard_links = 0;
    let mut links: HashMap<(u64, u64), HardLinkGroup> = HashMap::new();
    let mut pending_link_bytes = 0_u64;
    let directory_metadata = query.selection.min_size.is_none() && query.selection.older_than.is_none();
    let mut pruned_directories = 0;
    // The indexed stream already orders measurements by yield. Deferring a
    // high-yield candidate because its chart branch is nearly complete would
    // force lower-yield work elsewhere before satisfying the global bound.
    let mut deferred = Vec::new(); let mut branch_pruning = !indexed;
    let mut previous_remaining = u64::MAX; let mut checkpoint_directories = 0;
    let mut ranked_directories = 0;
    let mut file_hints = HashMap::<PathBuf, FileHints>::new();
    let mut report_nodes = HashMap::<PathBuf, NodeId>::new();
    let (errors, _) = walk(root, options, collector, |mut batch, queue| {
        if batch.node == usize::MAX {
            let relative = batch.directory.path.strip_prefix(root).unwrap_or(&batch.directory.path);
            batch.node = report_node(&mut tree, &mut report_nodes, relative, report_depth);
        }
        tree.nodes[batch.node].pending_batches += batch.new_batches;
        batches_seen += 1; continuation_batches += usize::from(!batch.directory_visit);
        let accounting_started = profiling.then(std::time::Instant::now);
        if !indexed && batch.directory_visit { directories += 1; }
        scanned_files += batch.files.scanned; selected_files += batch.files.selected as usize;
        if batch.files.has_large_files { tree.mark_large(batch.node); }
        if !batch.directory_visit { tree.nodes[batch.node].pending_batches -= batch.units; }
        tree.nodes[batch.node].pending_batches += batch.continuations.len();
        tree.nodes[batch.node].listed |= batch.directory_visit;
        tree.nodes[batch.node].completed = tree.nodes[batch.node].listed && tree.nodes[batch.node].pending_batches == 0;
        let relative = batch.directory.path.strip_prefix(root).unwrap_or(&batch.directory.path);
        let exact_node = !batch.probe || relative.components().count() <= report_depth;
        if batch.directory_inode != 0 && exact_node { tree.nodes[batch.node].inode = batch.directory_inode; }
        let new_metadata = !indexed && !tree.nodes[batch.node].metadata_counted;
        let mut probe_metadata_bytes = 0;
        if indexed {
            for (inode, bytes) in batch.measured_directories {
                if inode != 0 && measured_directory_metadata.insert(inode) {
                    directories += 1; probe_metadata_bytes += bytes;
                    if let Some(checks) = &allocation_checks { checks.record(inode); }
                    if let Some(bound) = &mut bound { bound.record(inode, bytes); }
                }
            }
        }
        if batch.directory_visit {
            let metadata_bytes = if new_metadata && directory_metadata && query.accounting == SizeAccounting::Allocated { batch.directory.allocated } else { 0 };
            tree.observe(batch.node, batch.files.bytes + metadata_bytes, batch.files.selected,
                directory_metadata || query.presentation.include_zeroes || batch.files.selected > 0);
        } else if batch.files.selected > 0 || (batch.probe && batch.directory_inode != 0) || probe_metadata_bytes > 0 {
            let metadata_bytes = probe_metadata_bytes + if batch.probe && new_metadata { batch.directory.allocated } else { 0 };
            tree.add_usage(batch.node, batch.files.bytes + metadata_bytes, batch.files.selected);
        }
        if !batch.files.big_names.is_empty() {
            let relative = batch.directory.path.strip_prefix(root).unwrap_or(&batch.directory.path).to_path_buf();
            let group = file_hints.entry(relative.clone()).or_insert_with(|| FileHints { directory: relative, names: Vec::new(), prior_bytes: 0 });
            group.names.extend(batch.files.big_names); group.prior_bytes = group.prior_bytes.saturating_add(batch.files.big_bytes);
        }
        if collect_hints {
            let bytes = batch.files.bytes;
            tree.nodes[batch.node].largest_direct_hint += bytes;
            // Every freshly measured file incurred a metadata operation,
            // including files read from a deferred group. Omitting those
            // costs makes a dense tiny-file directory look like a huge yield.
            tree.nodes[batch.node].hint_cost += batch.files.scanned as u64
                + if batch.directory_visit { 2 + batch.subdirs.len() as u64 } else { 0 };
            if batch.directory_visit && bytes >= 32 * 1024 && (directory_hints.len() < 4096 || directory_hints.peek().is_some_and(|hint: &std::cmp::Reverse<(u64, PathBuf)>| hint.0.0 < bytes)) {
                let relative = batch.directory.path.strip_prefix(root).unwrap_or(&batch.directory.path).to_path_buf();
                directory_hints.push(std::cmp::Reverse((bytes, relative)));
                if directory_hints.len() > 4096 { directory_hints.pop(); }
            }
        }
        let mut settled_usage: Option<(NodeId, u64, u64)> = None;
        for fact in batch.files.hard_links {
            let group = match links.entry((fact.device, fact.inode)) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    if bound_device == Some(fact.device) {
                        if let Some(checks) = &allocation_checks { checks.record(fact.inode); }
                        if let Some(bound) = &mut bound { bound.record(fact.inode, fact.allocated); }
                        pending_link_bytes += fact.allocated;
                    }
                    entry.insert(HardLinkGroup { fact, directory: batch.node, observed: 1, finalized: false, indexed_paths: None })
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    if indexed && !entry.get().finalized {
                        let group = entry.get_mut();
                        let fresh = match &mut group.indexed_paths {
                            Some(paths) => paths.insert(fact.path.clone()),
                            None => fact.path != group.fact.path,
                        };
                        if !fresh {
                            if let Some(bound) = &mut bound { bound.invalidate(); }
                            continue;
                        }
                        // Two-link groups settle on this observation, so their
                        // existing representative suffices without another
                        // allocation. Retain paths only when more work remains.
                        if group.indexed_paths.is_none() && (group.observed + 1 < group.fact.links
                            || !bound.as_ref().is_some_and(Bound::can_settle_links)) {
                            group.indexed_paths = Some(HashSet::from([group.fact.path.clone(), fact.path.clone()]));
                        }
                    }
                    duplicate_hard_links += 1;
                    entry.get_mut().observed += 1;
                    if (fact.allocated, fact.links) != (entry.get().fact.allocated, entry.get().fact.links) {
                        if let Some(bound) = &mut bound { bound.invalidate(); }
                    }
                    if fact.path.as_os_str() < entry.get().fact.path.as_os_str() {
                        entry.get_mut().fact = fact; entry.get_mut().directory = batch.node;
                    }
                    entry.into_mut()
                }
            };
            if let Some(bound) = &mut bound {
                if group.observed > group.fact.links && group.finalized {
                    bound.invalidate(); // An alias invalidates the allocation bound.
                } else if bound.can_settle_links() && group.observed == group.fact.links && !group.finalized {
                    // Before all links are seen, their whole allocation remains
                    // uncertain: an unseen smaller path could change attribution.
                    if bound_device == Some(group.fact.device) { pending_link_bytes = pending_link_bytes.saturating_sub(group.fact.allocated); }
                    if settled_usage.as_ref().is_some_and(|usage| usage.0 != group.directory) {
                        let (directory, bytes, files) = settled_usage.take().unwrap();
                        tree.add_usage(directory, bytes, files);
                    }
                    let usage = settled_usage.get_or_insert((group.directory, 0, 0));
                    usage.1 += group.fact.allocated; usage.2 += 1;
                    selected_files += 1; group.finalized = true; group.indexed_paths = None;
                }
            }
        }
        if let Some((directory, bytes, files)) = settled_usage { tree.add_usage(directory, bytes, files); }
        if let Some(bound) = &mut bound {
            if (batch.directory_visit || batch.probe) && batch.directory_inode != 0 && new_metadata {
                if let Some(checks) = &allocation_checks { checks.record(batch.directory_inode); }
                bound.record(batch.directory_inode, batch.directory.allocated);
            }
            bound.account_file_bytes(batch.files.bound_bytes);
            for (inode, bytes) in batch.files.allocations { bound.record(inode, bytes); }
        }
        if exact_node && (batch.directory_visit || batch.probe) && batch.directory_inode != 0 { tree.nodes[batch.node].metadata_counted = true; }
        if profiling && batch.probe && (directories <= 20 || directories % 50_000 == 0) {
            eprintln!("dq[profile]: indexed directory={} path={} batch-bytes={} measured={} remaining={:?} pending-links={pending_link_bytes}",
                directories, batch.directory.path.display(), batch.files.bytes, tree.nodes[0].bytes, bound.as_ref().and_then(Bound::remaining));
        }
        if let Some(started) = accounting_started { accounting_time += started.elapsed(); }
        let scheduling_started = profiling.then(std::time::Instant::now);
        let mut child_metadata_bytes = 0;
        let parent_closed = tree.is_closed(batch.node);
        for mut work in batch.subdirs {
            let relative = if tree.nodes[batch.node].depth < report_depth {
                work.path.strip_prefix(root).unwrap_or(&work.path)
            } else { Path::new("") };
            work.node = tree.add_child(batch.node, relative);
            if tree.nodes[work.node].scheduled { continue; }
            tree.nodes[work.node].scheduled = true;
            tree.nodes[work.node].inode = work.metadata.as_ref().map_or(0, |metadata| metadata.inode);
            if let (Some(bound), Some(metadata)) = (&mut bound, &work.metadata) {
                // A listed child already has fresh inode metadata. Credit its
                // allocation now; opening the child again adds only its files.
                let fresh = if !indexed { !tree.nodes[work.node].metadata_counted }
                    else { measured_directory_metadata.insert(metadata.inode) };
                if fresh {
                    if let Some(checks) = &allocation_checks { checks.record(metadata.inode); }
                    bound.record(metadata.inode, metadata.allocated);
                    tree.precount_child(work.node, metadata.allocated);
                    child_metadata_bytes += metadata.allocated;
                    tree.nodes[work.node].metadata_counted = true;
                }
            }
            if parent_closed { pruned_directories += 1; deferred.push(work); }
            else {
                let priority = if queue.buckets.is_some() { priority(&work, &tree) } else { 0 };
                queue.push_ranked(work, priority);
            }
        }
        if child_metadata_bytes > 0 { tree.add_descendant_usage(batch.node, child_metadata_bytes, 0); }
        for mut work in batch.continuations {
            work.node = batch.node;
            if parent_closed { pruned_directories += 1; deferred.push(work); }
            else { let rank = priority(&work, &tree); queue.push_ranked(work, rank); }
        }
        if !batch.finished_job {
            if let Some(started) = scheduling_started { scheduling_time += started.elapsed(); }
            return;
        }
        if let Some(bound) = &mut bound {
            if allocation_checks.as_ref().is_some_and(|checks| !checks.valid()) { bound.invalidate(); }
            // The global stop already covered all pending work. Finish the
            // in-flight reads, then validate once more before reporting.
            if tree.is_closed(0) { return; }
            let Some(remaining) = bound.remaining() else { return; };
            // Revisit the schedule when the bound tightens, or when many cheap
            // directories failed to yield bytes. Avoid sorting after every batch.
            // Leave a little room for allocation changes while in-flight
            // directory reads finish; the final check uses the full budget.
            let stop_at = bound.capacity as f64 * query.traversal.max_error_percent / 100.0 * 0.99;
            let within_budget = remaining.saturating_add(pending_link_bytes) as f64 <= stop_at;
            let tightened = remaining < previous_remaining.saturating_mul(4) / 5;
            if directories == 1 || within_budget || tightened
                || directories - checkpoint_directories >= 16_384 {
                bound.refresh(root);
                let Some(remaining) = bound.remaining() else { return; };
                previous_remaining = remaining; checkpoint_directories = directories;
                let stop_at = bound.capacity as f64 * query.traversal.max_error_percent / 100.0 * 0.99;
                let mut changed = false;
                if remaining.saturating_add(pending_link_bytes) as f64 <= stop_at {
                    // One shared budget covers every unfinished branch and any
                    // inaccessible, deleted-open, or other unassigned allocation.
                    changed = !tree.is_closed(0);
                    tree.close(0);
                    if queue.candidates.take().is_some() { pruned_directories += 1; }
                } else if branch_pruning {
                    for id in pruning::closed_branches(&tree, query, remaining.saturating_add(pending_link_bytes)) {
                        changed |= !tree.is_closed(id); tree.close(id);
                    }
                }
                // Bound checks do not require touching the entire frontier.
                // Re-rank after substantial byte progress or occasionally to
                // let stale yield estimates decay; remove newly closed work
                // immediately. Each new directory gets a current rank anyway.
                if changed || tightened || directories - ranked_directories >= 131_072 {
                    queue.update(|mut work| {
                        if !work.registered {
                            if work.node == usize::MAX {
                                let relative = work.path.strip_prefix(root).unwrap_or(&work.path);
                                work.node = report_node(&mut tree, &mut report_nodes, relative, report_depth);
                            }
                            tree.nodes[work.node].pending_batches += 1;
                            work.registered = true;
                        }
                        if tree.is_closed(work.node) {
                            pruned_directories += 1; deferred.push(work); None
                        } else {
                            let rank = priority(&work, &tree);
                            Some((work, rank))
                        }
                    });
                    ranked_directories = directories;
                }
            }
            // Visibility-based pruning prioritizes work; it must not leave
            // the final partial result above the requested global error budget.
            if queue.is_empty() && !deferred.is_empty()
                && bound.remaining().is_some_and(|remaining| remaining.saturating_add(pending_link_bytes) as f64
                    > bound.capacity as f64 * query.traversal.max_error_percent / 100.0) {
                tree.reopen(); branch_pruning = false; pruned_directories = 0;
                for work in deferred.drain(..) {
                    let rank = if queue.buckets.is_some() { priority(&work, &tree) } else { 0 };
                    queue.push_ranked(work, rank);
                }
            }
        }
        if let Some(started) = scheduling_started { scheduling_time += started.elapsed(); }
    });
    if profiling {
        eprintln!("dq[profile]: accounting={}ms scheduling={}ms", accounting_time.as_millis(), scheduling_time.as_millis());
        eprintln!("dq[profile]: batches={batches_seen} continuations={continuation_batches}");
    }
    let finalization_started = profiling.then(std::time::Instant::now);
    let link_groups = links.len();
    if profiling {
        let pending_links = links.values().filter(|group| !group.finalized).map(|group| group.fact.allocated).sum::<u64>();
        eprintln!("dq[profile]: pending-link-bytes={pending_links}");
    }
    // Deduplicate before applying file filters, just as the detailed report does.
    for group in links.into_values() {
        // Aliases are not independent byte-yield candidates. Give each shared
        // physical inode one scheduling representative, including groups whose
        // final directory attribution remains unsettled in the partial report.
        if collect_hints { tree.nodes[group.directory].largest_direct_hint += group.fact.allocated; }
        // An unseen link could be a lexicographically earlier representative.
        // Keep its allocation unresolved rather than overstate a measured row.
        if group.finalized || pruned_directories > 0 { continue; }
        let fact = group.fact;
        let bytes = match query.accounting { SizeAccounting::Allocated => fact.allocated, SizeAccounting::Apparent => fact.apparent };
        if !selection.matches(bytes, fact.modified) { continue; }
        selected_files += 1;
        tree.add_usage(group.directory, bytes, 1);
    }
    if let Some(bound) = &mut bound { bound.refresh(root); }
    if allocation_checks.as_ref().is_some_and(|checks| !checks.valid()) {
        if let Some(bound) = &mut bound { bound.invalidate(); }
    }
    let invalid_bound = bound.as_ref().is_some_and(|bound| bound.remaining().is_none());
    let exceeded_budget = pruned_directories > 0 && bound.as_ref().is_some_and(|bound| bound.remaining().is_none_or(|remaining|
        remaining.saturating_add(pending_link_bytes) as f64 > bound.capacity as f64 * query.traversal.max_error_percent / 100.0));
    if (indexed && invalid_bound) || exceeded_budget || (indexed && pruned_directories == 0) {
        // Invalid bounds and an exhausted index require a fresh complete walk.
        // Rebuild the address index too, including newly created files.
        let mut complete = summarize_with_sources(root, query, None, &[], Arc::default(), remember, None);
        complete.duration_ms = started.elapsed().as_millis();
        return complete;
    }
    tree.finish_deep_totals();
    let levels = tree.levels(&depths).into_iter().map(|groups|
        groups.into_iter().map(|(relative, usage)| (root.join(relative), usage)).collect()).collect();
    if let Some(started) = finalization_started {
        eprintln!("dq[profile]: finalization={}ms hard-link-groups={link_groups}", started.elapsed().as_millis());
    }
    let unmeasured_bytes_upper_bound = if pruned_directories > 0 {
        bound.as_ref().and_then(Bound::remaining)
    } else { None };
    let directory_hints = directory_hints.into_sorted_vec().into_iter()
        .map(|std::cmp::Reverse((bytes, path))| (path, bytes)).collect();
    let mut small_files = index.as_ref().map_or_else(SmallFiles::default, |index| index.snapshot());
    if collect_hints {
        small_files.carry_directory_weights(&initial_small);
        for node in &tree.nodes {
            if node.inode != 0 {
                let small = if node.has_large_files { Some(false) } else { node.completed.then_some(true) };
                small_files.remember_directory(node.inode, node.largest_direct_hint, small, !node.completed);
            }
        }
    }
    let known_unassigned_bytes = if pruned_directories > 0 { pending_link_bytes } else { 0 };
    let mut groups = file_hints.into_values().map(|group| {
        let count = super::summary_cache::names(&group.names).count();
        (count, group)
    }).collect::<Vec<_>>();
    groups.sort_unstable_by(|(a_count, a), (b_count, b)| {
        let a_cost = *a_count as u64 + 8;
        let b_cost = *b_count as u64 + 8;
        (u128::from(b.prior_bytes) * u128::from(a_cost)).cmp(&(u128::from(a.prior_bytes) * u128::from(b_cost)))
            .then_with(|| a.directory.cmp(&b.directory))
    });
    let count = groups.iter().map(|(count, _)| *count).sum::<usize>();
    let file_hints = if count <= super::summary_cache::MAX_FILE_HINTS { groups.into_iter().map(|(_, group)| group).collect() } else { Vec::new() };
    SummaryScan { root: root.to_path_buf(), levels, scanned_files, selected_files, directories, duplicate_hard_links,
        errors, duration_ms: started.elapsed().as_millis(), pruned_directories, unmeasured_bytes_upper_bound, known_unassigned_bytes,
        hints: directory_hints, file_hints, small_files }
}

fn report_prefix(relative: &Path, depth: usize) -> &Path {
    // Borrow the first chart components directly from the path. The lazy
    // stream can contain hundreds of thousands of directories but only a few
    // hundred chart ancestors; build each of those ancestor chains once.
    let mut components = relative.components();
    for _ in 0..depth { components.next(); }
    let suffix = components.as_path().as_os_str().as_bytes();
    let bytes = relative.as_os_str().as_bytes();
    let end = bytes.len() - suffix.len();
    Path::new(OsStr::from_bytes(bytes[..end].strip_suffix(b"/").unwrap_or(&bytes[..end])))
}

fn report_node(tree: &mut DirectoryTree, known: &mut HashMap<PathBuf, NodeId>, relative: &Path, depth: usize) -> NodeId {
    let prefix = report_prefix(relative, depth);
    if let Some(&id) = known.get(prefix) { return id; }
    let id = tree.ensure_path(prefix);
    known.insert(prefix.to_path_buf(), id);
    id
}

fn priority(work: &Work, tree: &DirectoryTree) -> Priority {
    if work.probe { return 254; }
    if let Some(pending) = &work.pending {
        return 29 + (pending.len().max(1).ilog2() / 5).min(2) as Priority;
    }
    let depth = tree.nodes[work.node].depth;
    // Establish all three chart levels before pursuing deeper detail. Then
    // combine directory-listing size with the observed yield of its branch.
    // These are ordering hints only, never evidence that a branch is small.
    if depth == 0 { return 255; }
    let shallow = if depth <= 3 { 4 - depth } else { 0 };
    if shallow == 0 && work.small_branch {
        return (work.metadata.as_ref().map_or(4096, |item| item.apparent).max(1).ilog2() / 5).min(7) as Priority;
    }
    if work.prior_bytes > 0 {
        // Prior measurements guide order only. Every byte contributing to the
        // bound or chart is still measured in this invocation.
        let exponent = work.prior_bytes.max(4096).ilog2();
        // Keep gigabyte branches distinct within each depth tier.
        let score = (32 + exponent.saturating_sub(12) as usize).min(63);
        return (shallow * 64 + score) as Priority;
    }
    let mut ancestor = tree.branch(work.node);
    let mut average = 4096;
    while ancestor != 0 {
        let node = &tree.nodes[ancestor];
        if let Some(observed) = node.bytes.checked_div(node.visited) { average = observed; break; }
        ancestor = node.parent;
    }
    let listing_bytes = work.metadata.as_ref().map_or(4096, |metadata| metadata.apparent);
    let score = listing_bytes.saturating_mul(u64::from(average.max(1).ilog2())).max(1);
    (shallow * 64 + score.ilog2().clamp(16, 31) as usize) as Priority
}

fn walk<C: CollectFiles>(root: &Path, options: WalkOptions<'_>, collector: C,
    mut receive: impl FnMut(Batch<C>, &mut WorkQueue)) -> (Vec<String>, u128) {
    const DIRECTORIES_PER_JOB: usize = 16;
    let started = std::time::Instant::now();
    let root_device = fs::metadata(root).map(|metadata| metadata.dev()).unwrap_or(0);
    let pool = ThreadPool::new(options.threads); let (tx, rx) = std::sync::mpsc::channel();
    let root = Arc::new(root.to_path_buf()); let excludes: Arc<[String]> = options.excludes.to_vec().into();
    let mut hints = HashMap::<PathBuf, u64>::new();
    for (path, bytes) in options.hints.iter().take(4096) {
        if !path.components().all(|component| matches!(component, std::path::Component::Normal(_))) { continue; }
        for ancestor in path.ancestors() {
            let total = hints.entry(ancestor.to_path_buf()).or_default();
            *total = total.saturating_add(*bytes);
        }
    }
    let streaming_index = options.candidates.is_some();
    let read_options = ReadOptions { root: Arc::clone(&root), root_device, one_file_system: options.one_file_system, excludes,
        hints: Arc::new(hints), small: options.small, index: options.index, adaptive: options.adaptive,
        root_directory: options.root_directory, streaming: streaming_index };
    let mut queue = WorkQueue::new(options.adaptive);
    queue.candidates = options.candidates; queue.candidate_root = root.as_ref().clone();
    // If the lazy index cannot meet the live allocation bound, summarize
    // performs a fresh complete traversal. Keeping two overlapping walks here
    // would require retaining every indexed name merely to suppress duplicates.
    if !streaming_index {
        queue.push_ranked(Work { path: root.as_ref().clone(), metadata: fs::metadata(root.as_path()).ok().as_ref().map(EntryMetadata::from),
            node: 0, prior_bytes: 0, pending: None, small_branch: false, probe: false, registered: true, descriptor: None }, 255);
    }
    let mut in_flight = 0usize;
    let mut progress = Progress::new(options.show_progress); let mut errors = Vec::new();
    let profiling = std::env::var_os("QTOOLS_PROFILE").is_some();
    let mut dispatch_time = std::time::Duration::ZERO;
    let mut waiting_time = std::time::Duration::ZERO;
    while !queue.is_empty() || in_flight > 0 {
        let dispatch_started = profiling.then(std::time::Instant::now);
        while in_flight < options.threads.saturating_mul(2) {
            let Some(directory) = queue.pop_front() else { break };
            let mut directories = vec![directory];
            for _ in 1..DIRECTORIES_PER_JOB {
                let Some(directory) = queue.pop_front() else { break };
                directories.push(directory);
            }
            submit(directories, read_options.clone(), collector.clone(), &pool, tx.clone());
            in_flight += 1;
        }
        if let Some(started) = dispatch_started { dispatch_time += started.elapsed(); }
        // The lazy reader can exhaust or reject its first record before a
        // job is submitted. There is then no worker result to wait for.
        if in_flight == 0 { break; }
        let waiting_started = profiling.then(std::time::Instant::now);
        let batches = match rx.recv() { Ok(batches) => batches, Err(_) => { errors.push("scan worker channel closed unexpectedly".into()); break } };
        if let Some(started) = waiting_started { waiting_time += started.elapsed(); }
        in_flight -= 1;
        let completed = batches.iter().map(|batch| if batch.probe { batch.units } else { usize::from(batch.directory_visit) }).sum();
        let scanned = batches.iter().map(|batch| batch.scanned).sum();
        let last = batches.len() - 1;
        for (index, mut batch) in batches.into_iter().enumerate() {
            batch.finished_job = index == last;
            errors.append(&mut batch.errors); receive(batch, &mut queue);
        }
        progress.update(completed, queue.len() + in_flight * DIRECTORIES_PER_JOB, scanned);
    }
    progress.finish();
    if profiling {
        eprintln!("dq[profile]: dispatch={}ms waiting={}ms", dispatch_time.as_millis(), waiting_time.as_millis());
    }
    (errors, started.elapsed().as_millis())
}

fn submit<C: CollectFiles>(work: Vec<Work>, options: ReadOptions, collector: C, pool: &ThreadPool, tx: Sender<Vec<Batch<C>>>) {
    // Amortize pool/channel wakeups over several small directories. Each still
    // produces its own batch, so bounds and report accounting stay unchanged.
    pool.execute(move || {
        let batches = work.into_iter().map(|work| read_directory_with(work, &options, collector.clone()));
        let batches = if options.streaming {
            // Combine measurements for the same chart ancestor on the worker.
            // The coordinator still checks every physical directory/inode,
            // but updates cumulative chart totals once per combined batch.
            let mut groups = HashMap::<PathBuf, usize>::new();
            let mut combined: Vec<Batch<C>> = Vec::new();
            for mut batch in batches {
                batch.measured_directories.push((batch.directory_inode, batch.directory.allocated));
                let relative = batch.directory.path.strip_prefix(options.root.as_path()).unwrap_or(&batch.directory.path);
                let prefix = report_prefix(relative, 3);
                if let Some(&index) = groups.get(prefix) {
                    let total = &mut combined[index];
                    if total.node == usize::MAX { total.node = batch.node; }
                    total.units += batch.units; total.new_batches += batch.new_batches; total.scanned += batch.scanned;
                    total.measured_directories.append(&mut batch.measured_directories);
                    total.continuations.append(&mut batch.continuations); total.errors.append(&mut batch.errors);
                    total.files.merge(batch.files);
                } else { groups.insert(prefix.to_path_buf(), combined.len()); combined.push(batch); }
            }
            combined
        } else { batches.collect() };
        let _ = tx.send(batches);
    });
}

fn read_directory_with<C: CollectFiles>(work: Work, options: &ReadOptions, files: C) -> Batch<C> {
    let ReadOptions { root, root_device, one_file_system, excludes, hints, .. } = options;
    let Work { path: directory, metadata, node, pending, probe, registered, descriptor, .. } = work;
    let directory_fact = DirectoryFact { path: PathBuf::new(),
        allocated: metadata.as_ref().map_or(0, |item| item.allocated),
        uid: metadata.as_ref().map_or(0, |item| item.uid) };
    let mut batch = Batch { directory: directory_fact, directory_inode: metadata.as_ref().map_or(0, |item| item.inode), node,
        subdirs: Vec::new(), files, errors: Vec::new(), scanned: 0, directory_visit: pending.is_none(), probe, continuations: Vec::new(), finished_job: false,
        units: 1, new_batches: usize::from(!registered), measured_directories: Vec::new() };
    if options.adaptive && (!options.small.is_empty() || pending.is_some()) {
        let mut batch = read_directory_deferred(&directory, metadata, pending, descriptor, options, batch);
        batch.directory.path = directory;
        return batch;
    }
    let entries = match fs::read_dir(&directory) { Ok(entries) => entries,
        Err(error) => { batch.errors.push(format!("{}: {error}", directory.display())); batch.directory.path = directory; return batch } };
    for result in entries {
        let entry = match result { Ok(entry) => entry, Err(error) => { batch.errors.push(format!("{}: {error}", directory.display())); continue } };
        if !excludes.is_empty() {
            let path = entry.path(); let relative = path.strip_prefix(root.as_path()).unwrap_or(&path).to_string_lossy();
            if excluded(&relative, excludes) || excluded(&entry.file_name().to_string_lossy(), excludes) { continue; }
        }
        let file_type = match entry.file_type() { Ok(kind) => kind, Err(error) => { batch.errors.push(format!("{}: {error}", entry.path().display())); continue } };
        if !file_type.is_dir() && !file_type.is_file() { continue; }
        let metadata = match entry.metadata() { Ok(metadata) => metadata, Err(error) => { batch.errors.push(format!("{}: {error}", entry.path().display())); continue } };
        if metadata.is_dir() { if should_descend(*one_file_system, *root_device, metadata.dev()) {
            let path = entry.path();
            let relative = path.strip_prefix(root.as_path()).unwrap_or(&path);
            let prior_bytes = options.small.directory_bytes(metadata.ino());
            let prior_bytes = if prior_bytes > 0 { prior_bytes } else { hints.get(relative).copied().unwrap_or(0) };
            let small_branch = options.small.directory_small(metadata.ino()) || (prior_bytes > 0 && prior_bytes <= 4096);
            batch.subdirs.push(Work { path, metadata: Some(EntryMetadata::from(&metadata)), node: 0, prior_bytes, pending: None, small_branch, probe: false, registered: true, descriptor: None });
        } continue; }
        if !metadata.is_file() || !should_descend(*one_file_system, *root_device, metadata.dev()) { continue; }
        let allocated = metadata.blocks().saturating_mul(512); batch.scanned = batch.scanned.saturating_add(allocated);
        if metadata.dev() == *root_device {
            if let Some(index) = &options.index { index.update(metadata.ino(), allocated <= 4096); }
        }
        batch.files.remember(&entry.file_name(), allocated);
        batch.files.push(|| entry.path(), &EntryMetadata::from(&metadata));
    }
    batch.directory.path = directory;
    batch
}

fn collect_native<C: CollectFiles>(directory: &Path, file: &fs::File, name: &CStr, options: &ReadOptions, batch: &mut Batch<C>) {
    let path = || directory.join(OsStr::from_bytes(name.to_bytes()));
    let item = match metadata_at(file, name) { Ok(item) => item,
        Err(error) => { batch.errors.push(format!("{}: {error}", path().display())); return; } };
    match item.mode & libc::S_IFMT {
        libc::S_IFDIR if should_descend(options.one_file_system, options.root_device, item.device) => {
            let path = path();
            let relative = path.strip_prefix(options.root.as_path()).unwrap_or(&path);
            let prior_bytes = options.small.directory_bytes(item.inode);
            let prior_bytes = if prior_bytes > 0 { prior_bytes } else { options.hints.get(relative).copied().unwrap_or(0) };
            let small_branch = options.small.directory_small(item.inode) || (prior_bytes > 0 && prior_bytes <= 4096);
            batch.subdirs.push(Work { path, metadata: Some(item), node: 0, prior_bytes, pending: None, small_branch, probe: false, registered: true, descriptor: None });
        }
        libc::S_IFREG if should_descend(options.one_file_system, options.root_device, item.device) => {
            batch.scanned = batch.scanned.saturating_add(item.allocated);
            if item.device == options.root_device {
                if let Some(index) = &options.index { index.update(item.inode, item.allocated <= 4096); }
            }
            if !batch.probe { batch.files.remember(OsStr::from_bytes(name.to_bytes()), item.allocated); }
            batch.files.push(path, &item);
        }
        _ => {}
    }
}

fn read_directory_deferred<C: CollectFiles>(directory: &Path, mut metadata: Option<EntryMetadata>, pending: Option<Vec<u8>>,
    descriptor: Option<Arc<fs::File>>, options: &ReadOptions, mut batch: Batch<C>) -> Batch<C> {
    use std::os::unix::fs::OpenOptionsExt;
    let flags = libc::O_DIRECTORY | if batch.node == 0 { 0 } else { libc::O_NOFOLLOW };
    let file = match descriptor {
        Some(file) => file,
        None => {
            let opened = if batch.probe { open_probe(directory, options) }
                else { fs::OpenOptions::new().read(true).custom_flags(flags).open(directory) };
            match opened { Ok(file) => Arc::new(file), Err(error) => { batch.errors.push(format!("{}: {error}", directory.display())); return batch; } }
        }
    };
    if batch.probe && metadata.is_none() {
        let item = match file.metadata() { Ok(item) => item, Err(error) => {
            batch.errors.push(format!("{}: {error}", directory.display())); return batch;
        } };
        if item.dev() != options.root_device { return batch; }
        batch.directory_inode = item.ino();
        batch.directory.allocated = item.blocks().saturating_mul(512);
        batch.directory.uid = item.uid();
        metadata = Some(EntryMetadata::from(&item));
    }
    let mut deferred = Vec::new();
    if let Some(mut pending) = pending {
        for _ in 0..256 {
            if pending.is_empty() { break; }
            let end = pending.len() - 2;
            let size = u16::from_ne_bytes(pending[end..].try_into().unwrap()) as usize;
            let start = end - size;
            let name = CStr::from_bytes_with_nul(&pending[start..end]).expect("stored name has its nul terminator");
            collect_native(directory, &file, name, options, &mut batch);
            pending.truncate(start);
        }
        deferred = pending;
    } else {
        let mut deferred_files = 0;
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
            // SAFETY: only the count bytes initialized by the successful
            // system call are read; the unused buffer tail stays untouched.
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
                let inode = u64::from_ne_bytes(record[..8].try_into().unwrap());
                if kind == libc::DT_REG && options.small.contains(inode) {
                    // This is a scheduling hint. Its old size is never added
                    // to a total; a grown file remains in the unseen bound.
                    deferred.extend_from_slice(name.to_bytes_with_nul());
                    deferred.extend_from_slice(&(name.to_bytes_with_nul().len() as u16).to_ne_bytes());
                    deferred_files += 1;
                } else { collect_native(directory, &file, name, options, &mut batch); }
            }
        }
        if deferred_files <= 16 {
            // Reopening a directory and coordinating another job costs more
            // than measuring a few tiny files while its descriptor is live.
            // This is batching policy, not a stopping condition or size bound.
            while !deferred.is_empty() {
                let end = deferred.len() - 2;
                let size = u16::from_ne_bytes(deferred[end..].try_into().unwrap()) as usize;
                let start = end - size;
                let name = CStr::from_bytes_with_nul(&deferred[start..end]).expect("stored name has its nul terminator");
                collect_native(directory, &file, name, options, &mut batch);
                deferred.truncate(start);
            }
        }
    }
    if !deferred.is_empty() {
        batch.continuations.push(Work { path: directory.to_path_buf(), metadata, node: batch.node, prior_bytes: 0,
            pending: Some(deferred), small_branch: true, probe: batch.probe, registered: true, descriptor: Some(file) });
    }
    batch
}

#[cfg(test)]
fn read_directory(directory: PathBuf, root: &Path, root_device: u64, one_file_system: bool, excludes: &[String]) -> Batch<Vec<FileFact>> {
    let metadata = fs::symlink_metadata(&directory).ok().as_ref().map(EntryMetadata::from);
    let options = ReadOptions { root: Arc::new(root.to_path_buf()), root_device, one_file_system,
        excludes: excludes.to_vec().into(), hints: Arc::new(HashMap::new()), small: Arc::default(), index: None, adaptive: false,
        root_directory: None, streaming: false };
    read_directory_with(Work { path: directory, metadata, node: 0, prior_bytes: 0, pending: None, small_branch: false, probe: false, registered: true, descriptor: None }, &options, Vec::new())
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
    #[test]
    fn indexed_measurements_revalidate_sizes_and_fall_back_for_unindexed_mass() {
        use clap::Parser;
        use crate::dq::cli::Args;
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("qtools-dq-index-{}-{unique}", std::process::id()));
        let leaf = root.join("code/sites/main");
        fs::create_dir_all(leaf.join("deep/more")).unwrap();
        fs::write(leaf.join("deep/more/big"), vec![1_u8; 16 * 1024 * 1024]).unwrap();
        fs::write(leaf.join("deep/more/shared"), vec![2_u8; 1024 * 1024]).unwrap();
        fs::hard_link(leaf.join("deep/more/shared"), leaf.join("alias")).unwrap();
        for number in 0..100 {
            let directory = leaf.join(format!("tail/{number}/nested"));
            fs::create_dir_all(&directory).unwrap(); fs::write(directory.join("tiny"), b"tiny").unwrap();
        }
        let query = DiskQuery::from_args(&Args::try_parse_from(["dq", "--threads", "2", "--noprogress"]).unwrap()).unwrap();
        let exact = summarize_with_bound(&root, &query, None);
        let bytes = exact.levels[0].values().map(|usage| usage.0).sum::<u64>();
        let mut learned = summarize_with_hints(&root, &query, Some(Bound::new(bytes + 100 * 1024 * 1024, bytes * 2)), &[], Arc::default(), true);
        assert_eq!(learned.levels, exact.levels);
        assert!(!learned.file_hints.is_empty());
        // Exercise the production lazy reader and check each chart ancestor's
        // allocation bound. There is no alternate in-memory index path.
        super::super::summary_cache::Cache::open(&root).unwrap().save(&learned);
        let mut cache = super::super::summary_cache::Cache::open(&root).unwrap();
        let candidates = cache.take_candidates();
        assert!(candidates.is_some());
        let streamed = summarize_with_sources(&root, &query, Some(Bound::new(bytes, bytes * 2).with_file_inode_checks(false)),
            &[], Arc::default(), false, candidates);
        assert!(streamed.directories < exact.directories / 2);
        let measured = streamed.levels[0].values().map(|usage| usage.0).sum::<u64>();
        assert!(measured <= bytes);
        assert!(bytes - measured <= streamed.unmeasured_bytes_upper_bound.unwrap_or(0) + streamed.known_unassigned_bytes);
        assert!(streamed.levels[2][&leaf].0 >= 16 * 1024 * 1024);
        for (measured, precise) in streamed.levels.iter().zip(&exact.levels) {
            for (path, usage) in measured { assert!(usage.0 <= precise.get(path).map_or(0, |usage| usage.0)); }
        }

        // A malformed first record must fall back rather than wait forever
        // on a worker channel to which no jobs have been submitted.
        let directory = learned.file_hints[0].directory.clone();
        for invalid in [PathBuf::from("../outside"), PathBuf::from("/outside"),
            PathBuf::from(OsStr::from_bytes(b"code/invalid\0path"))] {
            learned.file_hints[0].directory = invalid;
            super::super::summary_cache::Cache::open(&root).unwrap().save(&learned);
            let mut cache = super::super::summary_cache::Cache::open(&root).unwrap();
            let malformed = summarize_with_sources(&root, &query, Some(Bound::new(bytes, bytes * 2).with_file_inode_checks(false)),
                &[], Arc::default(), false, cache.take_candidates());
            assert_eq!(malformed.levels, exact.levels);
            assert_eq!(malformed.scanned_files, exact.scanned_files);
        }
        learned.file_hints[0].directory = directory;

        // A corrupt or duplicated address must invalidate the indexed result,
        // even when it would otherwise make the stopping budget look satisfied.
        learned.file_hints.insert(0, learned.file_hints[0].clone());
        super::super::summary_cache::Cache::open(&root).unwrap().save(&learned);
        let mut cache = super::super::summary_cache::Cache::open(&root).unwrap();
        let duplicated = summarize_with_sources(&root, &query, Some(Bound::new(bytes, bytes * 2).with_file_inode_checks(false)),
            &[], Arc::default(), false, cache.take_candidates());
        assert_eq!(duplicated.levels, exact.levels);
        assert_eq!(duplicated.scanned_files, exact.scanned_files);
        learned.file_hints.remove(0);
        super::super::summary_cache::Cache::open(&root).unwrap().save(&learned);

        // Shrink an indexed inode, and add large allocations in a branch that
        // the previous ordering considered insignificant.
        fs::write(leaf.join("deep/more/big"), b"small now").unwrap();
        fs::write(leaf.join("tail/0/nested/new"), vec![3_u8; 24 * 1024 * 1024]).unwrap();
        let exact = summarize_with_bound(&root, &query, None);
        let bytes = exact.levels[0].values().map(|usage| usage.0).sum::<u64>();
        let mut cache = super::super::summary_cache::Cache::open(&root).unwrap();
        let complete = summarize_with_sources(&root, &query,
            Some(Bound::new(bytes + 100 * 1024 * 1024, bytes * 2).with_file_inode_checks(false)),
            &learned.hints, Arc::new(learned.small_files.clone()), false, cache.take_candidates());
        assert_eq!(complete.levels, exact.levels);
        assert_eq!(complete.scanned_files, exact.scanned_files);
        assert_eq!(complete.selected_files, exact.selected_files);
        assert_eq!(complete.directories, exact.directories);
        assert!(complete.errors.is_empty());
        let mut cache = super::super::summary_cache::Cache::open(&root).unwrap();
        let bounded = summarize_with_sources(&root, &query, Some(Bound::new(bytes, bytes * 2).with_file_inode_checks(false)),
            &learned.hints, Arc::new(learned.small_files), false, cache.take_candidates());
        assert!(bounded.levels[2][&leaf].0 >= 24 * 1024 * 1024);
        let measured = bounded.levels[0].values().map(|usage| usage.0).sum::<u64>();
        assert!(measured <= bytes);
        assert!(bytes - measured <= bounded.unmeasured_bytes_upper_bound.unwrap_or(0) + bounded.known_unassigned_bytes);
        let mut cache = super::super::summary_cache::Cache::open(&root).unwrap();
        let complete = summarize_with_sources(&root, &query, Some(Bound::new(bytes + 100 * 1024 * 1024, bytes * 2).with_file_inode_checks(false)),
            &[], Arc::default(), true, cache.take_candidates());
        assert_eq!(complete.levels, exact.levels);
        assert_eq!(complete.scanned_files, exact.scanned_files);
        assert!(!complete.file_hints.is_empty(), "a full fallback must refresh the address index");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn repeated_indexed_hardlink_paths_cannot_settle_directory_attribution() {
        use clap::Parser;
        use crate::dq::cli::Args;
        use super::super::summary_cache::Cache;
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let pack = |names: &[&str]| {
            let mut packed = Vec::new();
            for name in names {
                packed.extend_from_slice(name.as_bytes()); packed.push(0);
                packed.extend_from_slice(&((name.len() + 1) as u16).to_ne_bytes());
            }
            packed
        };
        for links in [2, 3] {
            let root = std::env::temp_dir().join(format!("qtools-dq-link-index-{}-{unique}-{links}", std::process::id()));
            fs::create_dir_all(root.join("aaa")).unwrap(); fs::create_dir_all(root.join("zzz")).unwrap();
            fs::write(root.join("aaa/shared"), vec![1_u8; 16 * 1024 * 1024]).unwrap();
            fs::hard_link(root.join("aaa/shared"), root.join("zzz/alias-z")).unwrap();
            if links == 3 { fs::hard_link(root.join("aaa/shared"), root.join("zzz/alias-a")).unwrap(); }
            let query = DiskQuery::from_args(&Args::try_parse_from(["dq", "--threads", "1", "--noprogress"]).unwrap()).unwrap();
            let mut exact = summarize_with_hints(&root, &query, None, &[], Arc::default(), true);
            let bytes = exact.levels[0].values().map(|usage| usage.0).sum::<u64>();
            // Repeating alias-z must not count as discovering aaa/shared. In
            // the three-link case the representative changes to alias-a before
            // the duplicate appears, so comparing only the representative fails.
            exact.file_hints.retain(|group| group.directory == Path::new("zzz"));
            assert_eq!(exact.file_hints.len(), 1);
            exact.file_hints[0].names = if links == 2 { pack(&["alias-z"]) } else { pack(&["alias-a", "alias-z"]) };
            let mut repeated = exact.file_hints[0].clone(); repeated.names = pack(&["alias-z"]);
            exact.file_hints.push(repeated);
            Cache::open(&root).unwrap().save(&exact);
            let mut cache = Cache::open(&root).unwrap();
            let result = summarize_with_sources(&root, &query,
                Some(Bound::new(bytes, bytes * 2).with_file_inode_checks(false)),
                &[], Arc::default(), false, cache.take_candidates());
            assert_eq!(result.pruned_directories, 0);
            assert_eq!(result.levels, exact.levels);
            assert_eq!(result.scanned_files, exact.scanned_files);
            assert!(result.levels[0][&root.join("aaa")].0 >= 16 * 1024 * 1024);
            fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn untrusted_link_counts_keep_attribution_in_the_global_error_budget() {
        use clap::Parser;
        use crate::dq::cli::Args;
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("qtools-dq-link-counts-{}-{unique}", std::process::id()));
        fs::create_dir_all(root.join("code/sites/main")).unwrap();
        fs::write(root.join("shared"), vec![1_u8; 1024 * 1024]).unwrap();
        fs::hard_link(root.join("shared"), root.join("alias")).unwrap();
        fs::write(root.join("code/sites/main/big"), vec![2_u8; 64 * 1024 * 1024]).unwrap();
        for number in 0..200 {
            let tail = root.join(format!("code/sites/main/tail/{number}/nested"));
            fs::create_dir_all(&tail).unwrap(); fs::write(tail.join("tiny"), b"tiny").unwrap();
        }
        let query = DiskQuery::from_args(&Args::try_parse_from(["dq", "--threads", "1", "--noprogress"]).unwrap()).unwrap();
        let exact = summarize_with_bound(&root, &query, None);
        let bytes = exact.levels[0].values().map(|usage| usage.0).sum::<u64>();
        // Even after nlink paths are visited, aliases in an unverified mount
        // layout could have hidden a distinct link with an earlier path.
        let partial = summarize_with_bound(&root, &query,
            Some(Bound::new(bytes, bytes * 2).without_trusted_link_counts()));
        assert!(partial.pruned_directories > 0);
        assert_eq!(partial.known_unassigned_bytes, 1024 * 1024);
        assert_eq!(partial.levels[0][&root].1, 0);
        assert!(partial.levels[0][&root].0 < exact.levels[0][&root].0);
        let measured = partial.levels[0].values().map(|usage| usage.0).sum::<u64>();
        let uncertainty = partial.unmeasured_bytes_upper_bound.unwrap() + partial.known_unassigned_bytes;
        assert!(bytes - measured <= uncertainty);
        assert!(uncertainty as f64 <= bytes as f64 * 2.0 * query.traversal.max_error_percent / 100.0);
        fs::remove_dir_all(root).unwrap();
    }
    #[test] fn bucket_updates_preserve_every_eligible_directory_across_priority_boundaries() {
        let mut queue = WorkQueue::new(true);
        for node in 0..256 {
            queue.push_ranked(Work { path: PathBuf::from(node.to_string()), metadata: None, node, prior_bytes: 0,
                pending: None, small_branch: false, probe: false, registered: true, descriptor: None }, node as Priority);
        }
        assert_eq!(queue.len(), 256);
        assert_eq!(queue.pop_front().unwrap().node, 255);
        queue.update(|work| {
            if work.node % 3 == 0 { None }
            else { let rank = (255 - work.node) as Priority; Some((work, rank)) }
        });
        let expected = (0..255).filter(|node| node % 3 != 0).collect::<Vec<_>>();
        assert_eq!(queue.len(), expected.len());
        let actual = std::iter::from_fn(|| queue.pop_front()).map(|work| work.node).collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert!(queue.is_empty());
        // Equal-priority work must retain its order across a re-ranking pass;
        // reversing this queue measures the smallest candidates first.
        for node in 0..10 {
            queue.push_ranked(Work { path: PathBuf::new(), metadata: None, node, prior_bytes: 0,
                pending: None, small_branch: false, probe: false, registered: true, descriptor: None }, 100);
        }
        queue.update(|work| Some((work, 100)));
        let actual = std::iter::from_fn(|| queue.pop_front()).map(|work| work.node).collect::<Vec<_>>();
        assert_eq!(actual, (0..10).rev().collect::<Vec<_>>());
    }
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
    #[test] fn stale_small_file_and_branch_hints_cannot_hide_new_large_allocations() {
        use clap::Parser;
        use crate::dq::cli::Args;
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("qtools-dq-stale-hints-{}-{unique}", std::process::id()));
        let large = root.join("code/known/leaf");
        let thin = root.join("code/thin/leaf/deep");
        fs::create_dir_all(&large).unwrap(); fs::create_dir_all(&thin).unwrap();
        fs::write(large.join("big"), vec![1_u8; 32 * 1024 * 1024]).unwrap();
        for number in 0..200 {
            let directory = thin.join(format!("{number}/nested"));
            fs::create_dir_all(&directory).unwrap(); fs::write(directory.join("tiny"), b"tiny").unwrap();
        }
        let growing = thin.join("0/nested/tiny");
        let inode = fs::metadata(&growing).unwrap().ino();
        let query = DiskQuery::from_args(&Args::try_parse_from(["dq", "--threads", "1", "--noprogress"]).unwrap()).unwrap();
        let exact = summarize_with_bound(&root, &query, None);
        let bytes = exact.levels[0].values().map(|usage| usage.0).sum::<u64>();
        let learned = summarize_with_hints(&root, &query, Some(Bound::new(bytes + 64 * 1024 * 1024, bytes * 2)),
            &[], Arc::default(), true);
        assert_eq!(learned.levels, exact.levels);
        assert!(learned.small_files.contains(inode));
        let fast = summarize_with_hints(&root, &query, Some(Bound::new(bytes, bytes * 2)),
            &learned.hints, Arc::new(learned.small_files.clone()), false);
        assert!(fast.directories < exact.directories / 3);
        assert!(fast.scanned_files < exact.scanned_files / 3);

        // Grow the same inode inside a formerly small branch. Neither the
        // stale file bit nor the stale branch bit is a bound on its new size.
        fs::write(&growing, vec![2_u8; 48 * 1024 * 1024]).unwrap();
        assert_eq!(fs::metadata(&growing).unwrap().ino(), inode);
        let precise = summarize_with_bound(&root, &query, None);
        let bytes = precise.levels[0].values().map(|usage| usage.0).sum::<u64>();
        let fast = summarize_with_hints(&root, &query, Some(Bound::new(bytes, bytes * 2)),
            &learned.hints, Arc::new(learned.small_files.clone()), false);
        assert!(fast.levels[2][&root.join("code/thin/leaf")].0 >= 48 * 1024 * 1024);
        assert!(!fast.small_files.contains(inode));
        let measured = fast.levels[0].values().map(|usage| usage.0).sum::<u64>();
        assert!(measured <= bytes);
        assert!(bytes - measured <= fast.unmeasured_bytes_upper_bound.unwrap_or(0) + fast.known_unassigned_bytes);

        // An unreachable budget drains all deferred file groups. The native
        // reader must reproduce the ordinary precise walk exactly.
        let complete = summarize_with_hints(&root, &query, Some(Bound::new(bytes + 64 * 1024 * 1024, bytes * 2)),
            &learned.hints, Arc::new(learned.small_files), false);
        assert_eq!(complete.pruned_directories, 0);
        assert_eq!(complete.levels, precise.levels);
        assert_eq!(complete.scanned_files, precise.scanned_files);
        assert_eq!(complete.selected_files, precise.selected_files);
        assert_eq!(complete.directories, precise.directories);
        assert!(complete.errors.is_empty());
        fs::remove_dir_all(root).unwrap();
    }
    #[test] fn bounded_walk_skips_a_dominant_leaf_tail_without_losing_its_allocation() {
        use clap::Parser;
        use crate::dq::cli::Args;
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("qtools-dq-bound-{}-{unique}", std::process::id()));
        let leaf = root.join("code/sites/coalicionfloresta");
        fs::create_dir_all(&leaf).unwrap();
        fs::write(leaf.join("large"), vec![1_u8; 32 * 1024 * 1024]).unwrap();
        for number in 0..200 {
            let tail = leaf.join(format!("tail/{number}/nested"));
            fs::create_dir_all(&tail).unwrap(); fs::write(tail.join("small"), b"small").unwrap();
        }
        let query = DiskQuery::from_args(&Args::try_parse_from(["dq", "--threads", "1", "--noprogress"]).unwrap()).unwrap();
        let precise = summarize_with_bound(&root, &query, None);
        let exact_bytes = precise.levels[0].values().map(|usage| usage.0).sum::<u64>();
        let bounded = summarize_with_bound(&root, &query, Some(Bound::new(exact_bytes, exact_bytes * 2)));
        let measured = bounded.levels[0].values().map(|usage| usage.0).sum::<u64>();
        assert!(bounded.pruned_directories > 0);
        assert!(bounded.directories < precise.directories / 3, "{} vs {} directories", bounded.directories, precise.directories);
        assert!(measured <= exact_bytes);
        assert!(exact_bytes - measured <= bounded.unmeasured_bytes_upper_bound.unwrap());
        assert!(bounded.levels[2][&leaf].0 >= 32 * 1024 * 1024);

        let mut strict = query.clone(); strict.traversal.max_error_percent = 0.01;
        let bounded = summarize_with_bound(&root, &strict, Some(Bound::new(exact_bytes, exact_bytes * 2)));
        let measured = bounded.levels[0].values().map(|usage| usage.0).sum::<u64>();
        assert!((exact_bytes - measured) as f64 <= exact_bytes as f64 * 2.0 * 0.0001,
            "deferred branches must reopen when the global budget remains exceeded");
        let bounded = summarize_with_bound(&root, &strict, Some(Bound::new(exact_bytes + 10_000_000, exact_bytes * 2)));
        assert_eq!(bounded.pruned_directories, 0, "an unreachable budget must complete all accessible work");
        assert_eq!(bounded.levels, precise.levels);

        // Put a second large file beyond an initially empty-looking sibling.
        // The partition bound must keep that branch open until it is found.
        let hidden = root.join("code/sites/other/deep/nested");
        fs::create_dir_all(&hidden).unwrap();
        fs::write(hidden.join("unexpected"), vec![2_u8; 48 * 1024 * 1024]).unwrap();
        let precise = summarize_with_bound(&root, &query, None);
        let exact_bytes = precise.levels[0].values().map(|usage| usage.0).sum::<u64>();
        let bounded = summarize_with_bound(&root, &query, Some(Bound::new(exact_bytes, exact_bytes * 2)));
        assert!(bounded.levels[2][&root.join("code/sites/other")].0 >= 48 * 1024 * 1024);

        // Fully observed hard links can tighten the bound, but their allocation
        // must belong to the lexicographically first path, counted just once.
        fs::create_dir_all(root.join("aaa/deep")).unwrap();
        fs::hard_link(hidden.join("unexpected"), root.join("aaa/deep/shared")).unwrap();
        let precise = summarize_with_bound(&root, &query, None);
        let exact_bytes = precise.levels[0].values().map(|usage| usage.0).sum::<u64>();
        let bounded = summarize_with_bound(&root, &query, Some(Bound::new(exact_bytes, exact_bytes * 2)));
        assert!(bounded.levels[0][&root.join("aaa")].0 >= 48 * 1024 * 1024);
        assert!(bounded.levels[2][&root.join("code/sites/other")].0 < 48 * 1024 * 1024);
        let measured = bounded.levels[0].values().map(|usage| usage.0).sum::<u64>();
        assert!(measured <= exact_bytes);
        if let Some(upper) = bounded.unmeasured_bytes_upper_bound { assert!(exact_bytes - measured <= upper); }
        let aggregated = summarize_with_bound(&root, &query,
            Some(Bound::new(exact_bytes, exact_bytes * 2).with_file_inode_checks(false)));
        assert_eq!(aggregated.levels, bounded.levels,
            "batch allocation sums must preserve hard-link attribution and pruning bounds");

        // A link in an unfinished branch can change the representative. Its
        // allocation must stay unresolved until that lexically earlier path
        // is encountered, rather than overstate the already measured leaf.
        fs::write(leaf.join("pending"), vec![3_u8; 4 * 1024 * 1024]).unwrap();
        let mut pending = root.join("aaa/pending");
        for _ in 0..30 { pending.push("deep"); }
        fs::create_dir_all(&pending).unwrap();
        fs::hard_link(leaf.join("pending"), pending.join("shared")).unwrap();
        let precise = summarize_with_bound(&root, &query, None);
        let exact_bytes = precise.levels[0].values().map(|usage| usage.0).sum::<u64>();
        // Leave room for both this 4 MiB group and the tiny-file tail, so the
        // fixture can stop before the earlier link regardless of job timing.
        let mut pending_query = query.clone(); pending_query.traversal.max_error_percent = 5.0;
        let bounded = summarize_with_bound(&root, &pending_query, Some(Bound::new(exact_bytes, exact_bytes * 2)));
        assert!(bounded.pruned_directories > 0);
        assert_eq!(bounded.levels[0][&root.join("aaa")].1, 1,
            "the unfinished branch's second hard link should remain unseen");
        assert!(bounded.levels[2][&leaf].0 <= precise.levels[2][&leaf].0,
            "an unfinished hard-link group must not overstate its seen representative's directory");
        fs::remove_dir_all(&root).unwrap();
    }
}
