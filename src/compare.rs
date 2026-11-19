//! Folder comparison ("diff two folders") and copy between them.
//!
//! Left = the original/old side, Right = the new side:
//!   * only on the right  → **New**
//!   * only on the left   → **Deleted**
//!   * on both, different → **Edited**
//!
//! Files are compared by size + modification time (2 s tolerance, for FAT/SMB), or,
//! optionally, by content hash (xxh3) when sizes match.

use rayon::prelude::*;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Same,
    Edited,
    New,
    Deleted,
}

#[derive(Clone, Copy, Debug)]
pub struct Meta {
    pub is_dir: bool,
    pub size: u64,
    /// Unix seconds.
    pub mtime: i64,
}

#[derive(Clone, Debug)]
pub struct DNode {
    pub name: String,
    /// Path relative to both roots, `/`-separated ("" for the root).
    pub rel: String,
    pub left: Option<Meta>,
    pub right: Option<Meta>,
    pub status: Status,
    pub parent: usize,
    pub children: Vec<usize>,
    /// Counts of changed files below (for folders): new, deleted, edited.
    pub counts: [u32; 3],
    /// Bytes involved below: new (right size), deleted (left size), edited (right size).
    pub bytes: [u64; 3],
    /// Same content but different timestamp (only known with content compare).
    pub touched_only: bool,
}

impl DNode {
    pub fn is_dir(&self) -> bool {
        self.left.map(|m| m.is_dir).unwrap_or(false) || self.right.map(|m| m.is_dir).unwrap_or(false)
    }
    pub fn changed(&self) -> bool {
        self.status != Status::Same
    }
}

pub struct Diff {
    pub left: PathBuf,
    pub right: PathBuf,
    pub nodes: Vec<DNode>,
    pub content_compared: bool,
    pub elapsed_ms: u128,
}

#[derive(Default)]
pub struct Progress {
    pub listed: AtomicU64,
    pub hashed: AtomicU64,
    pub to_hash: AtomicU64,
    pub cancel: AtomicBool,
}

fn unix(t: std::io::Result<SystemTime>) -> i64 {
    t.ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Recursively list `root` (not following links): rel path → meta.
pub fn list(root: &Path, progress: &Progress) -> HashMap<String, Meta> {
    let out = parking_lot::Mutex::new(HashMap::new());
    fn visit<'s>(s: &rayon::Scope<'s>, dir: PathBuf, rel: String, out: &'s parking_lot::Mutex<HashMap<String, Meta>>, progress: &'s Progress) {
        if progress.cancel.load(Ordering::Relaxed) {
            return;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { return };
        let mut local = Vec::new();
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            let name = e.file_name().to_string_lossy().into_owned();
            let r = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            let is_link = md.file_type().is_symlink();
            let is_dir = md.is_dir() && !is_link;
            local.push((r.clone(), Meta { is_dir, size: if is_dir { 0 } else { md.len() }, mtime: unix(md.modified()) }));
            if is_dir {
                let p = e.path();
                s.spawn(move |s| visit(s, p, r, out, progress));
            }
        }
        progress.listed.fetch_add(local.len() as u64, Ordering::Relaxed);
        out.lock().extend(local);
    }
    rayon::scope(|s| visit(s, root.to_path_buf(), String::new(), &out, progress));
    out.into_inner()
}

pub fn hash_file(p: &Path) -> Option<u64> {
    let mut f = std::fs::File::open(p).ok()?;
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Some(h.digest())
}

pub fn compare(left: &Path, right: &Path, content: bool, progress: &Progress) -> Diff {
    let t0 = std::time::Instant::now();
    let (l, r) = rayon::join(|| list(left, progress), || list(right, progress));
    build(left, right, l, r, content, progress, t0)
}

fn build(left: &Path, right: &Path, l: HashMap<String, Meta>, r: HashMap<String, Meta>, content: bool, progress: &Progress, t0: std::time::Instant) -> Diff {
    // union of paths
    let mut rels: Vec<&String> = l.keys().chain(r.keys().filter(|k| !l.contains_key(*k))).collect();
    rels.sort_unstable();

    // which files need hashing
    let need_hash: Vec<&String> = if content {
        rels.iter()
            .copied()
            .filter(|k| matches!((l.get(*k), r.get(*k)), (Some(a), Some(b)) if !a.is_dir && !b.is_dir && a.size == b.size && a.size > 0))
            .collect()
    } else {
        Vec::new()
    };
    progress.to_hash.store(need_hash.len() as u64, Ordering::Relaxed);
    let same_content: HashMap<&String, bool> = need_hash
        .par_iter()
        .map(|k| {
            if progress.cancel.load(Ordering::Relaxed) {
                return (*k, false);
            }
            let rel = k.replace('/', std::path::MAIN_SEPARATOR_STR);
            let eq = match (hash_file(&left.join(&rel)), hash_file(&right.join(&rel))) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            };
            progress.hashed.fetch_add(1, Ordering::Relaxed);
            (*k, eq)
        })
        .collect();

    let mut nodes = vec![DNode {
        name: String::new(),
        rel: String::new(),
        left: Some(Meta { is_dir: true, size: 0, mtime: 0 }),
        right: Some(Meta { is_dir: true, size: 0, mtime: 0 }),
        status: Status::Same,
        parent: usize::MAX,
        children: Vec::new(),
        counts: [0; 3],
        bytes: [0; 3],
        touched_only: false,
    }];
    let mut index: HashMap<&str, usize> = HashMap::with_capacity(rels.len() + 1);
    index.insert("", 0);
    for k in &rels {
        let (parent_rel, name) = match k.rfind('/') {
            Some(p) => (&k[..p], &k[p + 1..]),
            None => ("", k.as_str()),
        };
        let parent = *index.get(parent_rel).unwrap_or(&0);
        let (a, b) = (l.get(*k).copied(), r.get(*k).copied());
        let (status, touched_only) = match (a, b) {
            (Some(_), None) => (Status::Deleted, false),
            (None, Some(_)) => (Status::New, false),
            (Some(a), Some(b)) => {
                if a.is_dir != b.is_dir {
                    (Status::Edited, false)
                } else if a.is_dir {
                    (Status::Same, false) // decided from children below
                } else if a.size != b.size {
                    (Status::Edited, false)
                } else if let Some(&eq) = same_content.get(k) {
                    if eq { (Status::Same, (a.mtime - b.mtime).abs() > 2) } else { (Status::Edited, false) }
                } else if (a.mtime - b.mtime).abs() > 2 {
                    (Status::Edited, false)
                } else {
                    (Status::Same, false)
                }
            }
            (None, None) => unreachable!(),
        };
        let idx = nodes.len();
        nodes.push(DNode {
            name: name.to_string(),
            rel: (*k).clone(),
            left: a,
            right: b,
            status,
            parent,
            children: Vec::new(),
            counts: [0; 3],
            bytes: [0; 3],
            touched_only,
        });
        nodes[parent].children.push(idx);
        index.insert(k.as_str(), idx);
    }
    drop(index);

    // aggregate bottom-up (children always have larger indices than parents)
    for i in (1..nodes.len()).rev() {
        let n = &nodes[i];
        let (mut c, mut by) = (n.counts, n.bytes);
        if !n.is_dir() {
            match n.status {
                Status::New => {
                    c[0] += 1;
                    by[0] += n.right.map(|m| m.size).unwrap_or(0);
                }
                Status::Deleted => {
                    c[1] += 1;
                    by[1] += n.left.map(|m| m.size).unwrap_or(0);
                }
                Status::Edited => {
                    c[2] += 1;
                    by[2] += n.right.map(|m| m.size).unwrap_or(0);
                }
                Status::Same => {}
            }
        }
        // folder totals: files add their size, sub-folders add their (already summed) totals
        let add_l = n.left.map(|m| m.size).unwrap_or(0);
        let add_r = n.right.map(|m| m.size).unwrap_or(0);
        let p = n.parent;
        let has_change = c.iter().any(|&x| x > 0);
        if n.is_dir() && n.status == Status::Same && has_change {
            nodes[i].status = Status::Edited;
        }
        let pn = &mut nodes[p];
        for k in 0..3 {
            pn.counts[k] += c[k];
            pn.bytes[k] += by[k];
        }
        if let Some(m) = pn.left.as_mut() {
            m.size += add_l;
        }
        if let Some(m) = pn.right.as_mut() {
            m.size += add_r;
        }
    }
    if nodes[0].counts.iter().any(|&x| x > 0) {
        nodes[0].status = Status::Edited;
    }
    // sort children: folders first, then by name (case-insensitive)
    let keys: Vec<(bool, String)> = nodes.iter().map(|n| (!n.is_dir(), n.name.to_lowercase())).collect();
    for n in nodes.iter_mut() {
        n.children.sort_by(|&a, &b| keys[a].cmp(&keys[b]));
    }
    Diff { left: left.to_path_buf(), right: right.to_path_buf(), nodes, content_compared: content, elapsed_ms: t0.elapsed().as_millis() }
}

impl Diff {
    pub fn abs(&self, right_side: bool, rel: &str) -> PathBuf {
        let base = if right_side { &self.right } else { &self.left };
        if rel.is_empty() { base.clone() } else { base.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR)) }
    }
}

// ---------------------------------------------------------------------------
// Copy
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct CopyJob {
    pub done_bytes: AtomicU64,
    pub total_bytes: AtomicU64,
    pub done_items: AtomicU64,
    pub cancel: AtomicBool,
    pub finished: AtomicBool,
    pub errors: parking_lot::Mutex<Vec<(String, String)>>,
}

/// Copy `rels` from one side to the other (folders recursively), overwriting.
/// Timestamps are preserved so a re-compare shows the items as identical.
pub fn copy_items(diff_left: PathBuf, diff_right: PathBuf, rels: Vec<String>, to_right: bool, job: std::sync::Arc<CopyJob>, ctx: egui::Context) {
    std::thread::Builder::new()
        .name("copy".into())
        .spawn(move || {
            let (src_root, dst_root) = if to_right { (&diff_left, &diff_right) } else { (&diff_right, &diff_left) };
            // total size up front for the progress bar
            let mut total = 0u64;
            for r in &rels {
                total += tree_size(&join_rel(src_root, r));
            }
            job.total_bytes.store(total, Ordering::Relaxed);
            for r in &rels {
                if job.cancel.load(Ordering::Relaxed) {
                    break;
                }
                let (s, d) = (join_rel(src_root, r), join_rel(dst_root, r));
                if let Err(e) = copy_recursive(&s, &d, &job, &ctx) {
                    job.errors.lock().push((r.clone(), e.to_string()));
                }
                job.done_items.fetch_add(1, Ordering::Relaxed);
            }
            job.finished.store(true, Ordering::SeqCst);
            ctx.request_repaint();
        })
        .expect("spawn copy thread");
}

fn join_rel(root: &Path, rel: &str) -> PathBuf {
    if rel.is_empty() { root.to_path_buf() } else { root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR)) }
}

fn tree_size(p: &Path) -> u64 {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::read_dir(p).map(|rd| rd.flatten().map(|e| tree_size(&e.path())).sum()).unwrap_or(0),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

fn copy_recursive(src: &Path, dst: &Path, job: &CopyJob, ctx: &egui::Context) -> std::io::Result<()> {
    if job.cancel.load(Ordering::Relaxed) {
        return Ok(());
    }
    let md = std::fs::symlink_metadata(src)?;
    if md.file_type().is_symlink() {
        return Ok(()); // never follow links
    }
    if md.is_dir() {
        // replacing a file with a folder
        if std::fs::symlink_metadata(dst).map(|m| !m.is_dir()).unwrap_or(false) {
            std::fs::remove_file(dst)?;
        }
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            if let Err(err) = copy_recursive(&e.path(), &dst.join(e.file_name()), job, ctx) {
                job.errors.lock().push((e.path().display().to_string(), err.to_string()));
            }
        }
        if let Ok(m) = md.modified() {
            let _ = std::fs::File::open(dst).and_then(|f| f.set_modified(m));
        }
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::symlink_metadata(dst).map(|m| m.is_dir()).unwrap_or(false) {
        std::fs::remove_dir_all(dst)?;
    }
    // overwrite read-only targets
    if let Ok(m) = std::fs::metadata(dst)
        && m.permissions().readonly()
    {
        let mut p = m.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        p.set_readonly(false);
        let _ = std::fs::set_permissions(dst, p);
    }
    let n = std::fs::copy(src, dst)?;
    if let Ok(m) = md.modified() {
        let _ = std::fs::OpenOptions::new().write(true).open(dst).and_then(|f| f.set_modified(m));
    }
    job.done_bytes.fetch_add(n, Ordering::Relaxed);
    ctx.request_repaint();
    Ok(())
}

// ---------------------------------------------------------------------------
// Text diff of two files
// ---------------------------------------------------------------------------

pub enum Line {
    Same(String),
    Removed(String),
    Added(String),
}

/// Line diff for text files up to 4 MB. Err with a reason for binary/large files.
pub fn text_diff(a: Option<&Path>, b: Option<&Path>) -> Result<Vec<Line>, String> {
    let read = |p: Option<&Path>| -> Result<String, String> {
        match p {
            None => Ok(String::new()),
            Some(p) => {
                let md = std::fs::metadata(p).map_err(|e| e.to_string())?;
                if md.len() > 4 << 20 {
                    return Err("file is larger than 4 MB".into());
                }
                let bytes = std::fs::read(p).map_err(|e| e.to_string())?;
                if bytes.iter().take(8000).any(|&c| c == 0) {
                    return Err("binary file".into());
                }
                Ok(String::from_utf8_lossy(&bytes).into_owned())
            }
        }
    };
    let (sa, sb) = (read(a)?, read(b)?);
    let d = similar::TextDiff::from_lines(&sa, &sb);
    Ok(d.iter_all_changes()
        .map(|c| {
            let s = c.to_string_lossy().trim_end_matches(['\n', '\r']).to_string();
            match c.tag() {
                similar::ChangeTag::Equal => Line::Same(s),
                similar::ChangeTag::Delete => Line::Removed(s),
                similar::ChangeTag::Insert => Line::Added(s),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_and_copy_roundtrip() {
        let base = std::env::temp_dir().join(format!("sm_cmp_{}", std::process::id()));
        let (l, r) = (base.join("L"), base.join("R"));
        let _ = std::fs::remove_dir_all(&base);
        for (p, c) in [("same.txt", "x"), ("edit.txt", "old"), ("gone.txt", "bye"), ("sub/deep.txt", "d")] {
            let f = l.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, c).unwrap();
        }
        std::fs::create_dir_all(r.join("sub")).unwrap();
        // copy same.txt with identical mtime so it is "Same"
        std::fs::copy(l.join("same.txt"), r.join("same.txt")).unwrap();
        let m = std::fs::metadata(l.join("same.txt")).unwrap().modified().unwrap();
        std::fs::OpenOptions::new().write(true).open(r.join("same.txt")).unwrap().set_modified(m).unwrap();
        std::fs::write(r.join("edit.txt"), "new!").unwrap();
        std::fs::write(r.join("fresh.txt"), "hi").unwrap();
        std::fs::copy(l.join("sub/deep.txt"), r.join("sub/deep.txt")).unwrap();
        let m2 = std::fs::metadata(l.join("sub/deep.txt")).unwrap().modified().unwrap();
        std::fs::OpenOptions::new().write(true).open(r.join("sub/deep.txt")).unwrap().set_modified(m2).unwrap();

        let p = Progress::default();
        let d = compare(&l, &r, true, &p);
        let st = |rel: &str| d.nodes.iter().find(|n| n.rel == rel).unwrap().status;
        assert_eq!(st("same.txt"), Status::Same);
        assert_eq!(st("edit.txt"), Status::Edited);
        assert_eq!(st("gone.txt"), Status::Deleted);
        assert_eq!(st("fresh.txt"), Status::New);
        assert_eq!(st("sub"), Status::Same);
        assert_eq!(d.nodes[0].counts, [1, 1, 1]);

        // copy everything left -> right, then only "fresh.txt" should differ
        let job = std::sync::Arc::new(CopyJob::default());
        copy_items(l.clone(), r.clone(), vec!["edit.txt".into(), "gone.txt".into()], true, job.clone(), egui::Context::default());
        while !job.finished.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let d2 = compare(&l, &r, true, &p);
        assert_eq!(d2.nodes[0].counts, [1, 0, 0], "{:?}", d2.nodes.iter().map(|n| (&n.rel, n.status)).collect::<Vec<_>>());

        let lines = text_diff(Some(&l.join("edit.txt")), Some(&r.join("fresh.txt"))).unwrap();
        assert!(lines.iter().any(|x| matches!(x, Line::Removed(_))));
        let _ = std::fs::remove_dir_all(&base);
    }
}
