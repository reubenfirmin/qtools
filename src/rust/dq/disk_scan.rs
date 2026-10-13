//! Concurrent filesystem traversal with detailed facts or streaming directory totals.
use std::path::{Path, PathBuf};

use super::hierarchy::Totals;

mod scheduling;
mod path_yield;
mod summary;
mod traversal;

pub(super) use summary::summarize;

#[derive(Debug, Clone)]
pub struct FileFact {
    pub path: PathBuf, pub apparent: u64, pub allocated: u64, pub device: u64,
    pub inode: u64, pub links: u64, pub uid: u32, pub modified: i64,
}
#[derive(Debug, Clone)]
pub struct DirectoryFact { pub path: PathBuf, pub allocated: u64, pub uid: u32 }
#[derive(Debug)]
pub struct Scan {
    pub root: PathBuf, pub files: Vec<FileFact>, pub directories: Vec<DirectoryFact>,
    pub errors: Vec<String>, pub duration_ms: u128,
}
pub(super) struct SummaryScan {
    pub root: PathBuf, pub levels: Vec<Totals>, pub scanned_files: usize, pub selected_files: usize,
    pub directories: usize, pub duplicate_hard_links: usize, pub errors: Vec<String>, pub duration_ms: u128,
    pub pruned_directories: usize, pub unmeasured_bytes_upper_bound: Option<u64>, pub known_unassigned_bytes: u64,
}

pub fn scan(root: &Path, threads: usize, one_file_system: bool, excludes: &[String], show_progress: bool) -> Scan {
    let mut files = Vec::new(); let mut directories = Vec::new();
    let options = traversal::WalkOptions { threads, one_file_system, excludes, show_progress, adaptive: false };
    let (errors, duration_ms) = traversal::walk(root, options, Vec::<FileFact>::new(), |event, queue| {
        if let traversal::WalkEvent::Batch(batch) = event {
            queue.extend(batch.subdirs); directories.push(batch.directory); files.extend(batch.files);
        }
    });
    Scan { root: root.to_path_buf(), files, directories, errors, duration_ms }
}

#[cfg(test)]
mod tests;
