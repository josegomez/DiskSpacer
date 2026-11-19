//! Duplicate file finder: size → head/tail hash → full hash (xxh3), all parallel.
//! Only the files that survive each stage are read further, so most data is never read.

use crate::model::{NONE, Tree, flags};
use rayon::prelude::*;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Clone, Debug)]
pub struct Group {
    pub size: u64,
    pub files: Vec<u32>,
}

impl Group {
    /// Bytes that deleting all but one copy would free.
    pub fn wasted(&self) -> u64 {
        self.size * (self.files.len() as u64 - 1)
    }
}

#[derive(Default)]
pub struct Progress {
    pub stage: AtomicU64, // 1 = grouping, 2 = quick hash, 3 = full hash
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
}

const SAMPLE: u64 = 64 * 1024;

fn quick_hash(p: &PathBuf, size: u64) -> Option<u64> {
    let mut f = std::fs::File::open(p).ok()?;
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0u8; SAMPLE as usize];
    let n = f.read(&mut buf).ok()?;
    h.update(&buf[..n]);
    if size > 2 * SAMPLE {
        f.seek(SeekFrom::Start(size - SAMPLE)).ok()?;
        let n = f.read(&mut buf).ok()?;
        h.update(&buf[..n]);
    }
    Some(h.digest())
}

/// Candidate groups (same size, ≥ 2 files) with their paths. Cheap: runs on the UI thread.
pub fn candidates(t: &Tree, scope: u32, min_size: u64) -> Vec<(u64, Vec<(u32, PathBuf)>)> {
    let mut by_size: HashMap<u64, Vec<u32>> = HashMap::new();
    t.for_each_descendant(scope, |i| {
        let n = t.node(i);
        if !n.is_dir() && n.size >= min_size.max(1) && n.flags & (flags::HARDLINK | flags::METAFILE | flags::OFFLINE | flags::REPARSE) == 0 {
            by_size.entry(n.size).or_default().push(i);
        }
    });
    by_size
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(size, ids)| (size, ids.into_iter().map(|i| (i, PathBuf::from(t.path(i)))).collect()))
        .collect()
}

/// Hash candidates (head/tail sample, then full) on a worker thread. Groups sorted by waste.
pub fn hash_groups(candidates: Vec<(u64, Vec<(u32, PathBuf)>)>, progress: &Progress) -> Vec<Group> {
    progress.stage.store(2, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);
    progress.total.store(candidates.iter().map(|c| c.1.len() as u64).sum(), Ordering::Relaxed);
    let stage2: Vec<(u64, Vec<(u32, PathBuf)>)> = candidates
        .par_iter()
        .flat_map_iter(|(size, files)| {
            let mut m: HashMap<u64, Vec<(u32, PathBuf)>> = HashMap::new();
            for (i, p) in files {
                if progress.cancel.load(Ordering::Relaxed) {
                    break;
                }
                if let Some(h) = quick_hash(p, *size) {
                    m.entry(h).or_default().push((*i, p.clone()));
                }
                progress.done.fetch_add(1, Ordering::Relaxed);
            }
            m.into_values().filter(|v| v.len() > 1).map(|v| (*size, v)).collect::<Vec<_>>()
        })
        .collect();

    // full hash (small files were fully covered by the head/tail sample)
    progress.stage.store(3, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);
    progress.total.store(stage2.iter().filter(|g| g.0 > 2 * SAMPLE).map(|g| g.0 * g.1.len() as u64).sum(), Ordering::Relaxed);
    let mut groups: Vec<Group> = stage2
        .par_iter()
        .flat_map_iter(|(size, files)| {
            if *size <= 2 * SAMPLE {
                return vec![Group { size: *size, files: files.iter().map(|f| f.0).collect() }];
            }
            let mut m: HashMap<u64, Vec<u32>> = HashMap::new();
            for (i, p) in files {
                if progress.cancel.load(Ordering::Relaxed) {
                    break;
                }
                if let Some(h) = crate::compare::hash_file(p) {
                    m.entry(h).or_default().push(*i);
                }
                progress.done.fetch_add(*size, Ordering::Relaxed);
            }
            m.into_values().filter(|v| v.len() > 1).map(|files| Group { size: *size, files }).collect()
        })
        .collect();
    groups.sort_by(|a, b| b.wasted().cmp(&a.wasted()));
    groups
}

/// Convenience for tests / CLI: candidates + hashing in one go.
#[allow(dead_code)]
pub fn find(t: &Tree, scope: u32, min_size: u64, progress: &Progress) -> Vec<Group> {
    let mut g = hash_groups(candidates(t, scope, min_size), progress);
    for x in g.iter_mut() {
        x.files.sort_by_key(|&i| std::cmp::Reverse(t.node(i).modified));
    }
    g
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    Newest,
    Oldest,
    ShortestPath,
}

/// Every file except the one to keep in each group.
pub fn auto_select(t: &Tree, groups: &[Group], keep: Keep) -> Vec<u32> {
    let mut out = Vec::new();
    for g in groups {
        let live: Vec<u32> = g.files.iter().copied().filter(|&i| !t.node(i).is_deleted() && t.node(i).parent != NONE).collect();
        if live.len() < 2 {
            continue;
        }
        let keeper = match keep {
            Keep::Newest => *live.iter().max_by_key(|&&i| t.node(i).modified).unwrap(),
            Keep::Oldest => *live.iter().min_by_key(|&&i| t.node(i).modified).unwrap(),
            Keep::ShortestPath => *live.iter().min_by_key(|&&i| t.path(i).len()).unwrap(),
        };
        out.extend(live.into_iter().filter(|&i| i != keeper));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{mft::Progress as P, walk};

    #[test]
    fn finds_real_duplicates() {
        let dir = std::env::temp_dir().join(format!("sm_dupes_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("one.bin"), &big).unwrap();
        std::fs::write(dir.join("a/two.bin"), &big).unwrap();
        std::fs::write(dir.join("a/b/three.bin"), &big).unwrap();
        let mut diff_mid = big.clone();
        diff_mid[150_000] ^= 1; // same size, same head & tail, different middle
        std::fs::write(dir.join("a/near.bin"), &diff_mid).unwrap();
        std::fs::write(dir.join("small1.txt"), b"hello").unwrap();
        std::fs::write(dir.join("small2.txt"), b"hello").unwrap();
        let (entries, ids) = walk::walk(&dir, &P::default());
        let t = crate::model::build_tree(entries, ids, 0, &dir.to_string_lossy(), crate::model::ScanMethod::Walk);
        let g = find(&t, t.root, 1, &Progress::default());
        assert_eq!(g.len(), 2, "{g:?}");
        assert_eq!(g[0].files.len(), 3);
        assert_eq!(g[0].wasted(), 600_000);
        assert_eq!(g[1].size, 5);
        let sel = auto_select(&t, &g, Keep::ShortestPath);
        assert_eq!(sel.len(), 3);
        assert!(!sel.iter().any(|&i| t.name(i) == "one.bin"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
