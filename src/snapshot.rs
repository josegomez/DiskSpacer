//! Saved scans: compact gzip'd binary snapshots of a tree, automatic history per drive,
//! and "what grew" comparison between two scans of the same place.

use crate::model::{NONE, RawEntry, ScanMethod, Tree, build_tree, flags};
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"SMSNAP01";

#[derive(Clone, Debug)]
pub struct Meta {
    /// Unix seconds when the scan was taken.
    pub taken: i64,
    /// Where it came from: "C:\", "/home/x", "user@host:/srv".
    pub label: String,
    pub files: u64,
    pub alloc: u64,
}

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("SizeMatters")
}

pub fn label_of(t: &Tree) -> String {
    match &t.remote {
        Some(c) => format!("{}:{}", c.target.label(), t.name(t.root)),
        None => t.name(t.root).to_string(),
    }
}

fn w_u64(w: &mut impl Write, v: u64) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
}
fn w_str(w: &mut impl Write, s: &str) -> std::io::Result<()> {
    let b = s.as_bytes();
    w.write_all(&(b.len().min(u16::MAX as usize) as u16).to_le_bytes())?;
    w.write_all(&b[..b.len().min(u16::MAX as usize)])
}
fn r_u64(r: &mut impl Read) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
fn r_u32(r: &mut impl Read) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
fn r_u16(r: &mut impl Read) -> std::io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}
fn r_str(r: &mut impl Read) -> std::io::Result<String> {
    let n = r_u16(r)? as usize;
    let mut b = vec![0u8; n];
    r.read_exact(&mut b)?;
    Ok(String::from_utf8_lossy(&b).into_owned())
}

/// Serialise the live part of a tree (uncompressed; fast enough for the UI thread).
pub fn encode(t: &Tree) -> Vec<u8> {
    let mut out = Vec::with_capacity(t.nodes.len() * 40);
    out.extend_from_slice(MAGIC);
    let now = chrono::Local::now().timestamp();
    let r = t.node(t.root);
    let _ = w_u64(&mut out, now as u64);
    let _ = w_str(&mut out, &label_of(t));
    let _ = w_str(&mut out, t.name(t.root));
    out.push(match t.method {
        ScanMethod::Mft => 0,
        ScanMethod::Walk => 1,
        ScanMethod::Ssh => 2,
    });
    out.push(t.sep as u8);
    let _ = w_u64(&mut out, t.volume_total);
    let _ = w_u64(&mut out, t.volume_free);
    let _ = w_u64(&mut out, r.files as u64);
    let _ = w_u64(&mut out, r.alloc);
    // pre-order walk: parents are always written before children
    let mut order = Vec::with_capacity(t.nodes.len());
    t.for_each_descendant(t.root, |i| order.push(i));
    let mut remap: HashMap<u32, u32> = HashMap::with_capacity(order.len());
    for (k, &i) in order.iter().enumerate() {
        remap.insert(i, k as u32);
    }
    out.extend_from_slice(&(order.len() as u32).to_le_bytes());
    for &i in &order {
        let n = t.node(i);
        let p = if i == t.root { 0 } else { remap.get(&n.parent).copied().unwrap_or(0) };
        out.extend_from_slice(&p.to_le_bytes());
        out.extend_from_slice(&(n.flags & !flags::DELETED).to_le_bytes());
        out.extend_from_slice(&(if n.is_dir() { 0 } else { n.size }).to_le_bytes());
        out.extend_from_slice(&(if n.is_dir() { 0 } else { n.alloc }).to_le_bytes());
        out.extend_from_slice(&n.created.to_le_bytes());
        out.extend_from_slice(&n.modified.to_le_bytes());
        out.extend_from_slice(&n.accessed.to_le_bytes());
        let _ = w_str(&mut out, if i == t.root { "" } else { t.name(i) });
    }
    out
}

pub fn write_compressed(bytes: &[u8], path: &Path) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let f = std::fs::File::create(&tmp)?;
        let mut gz = flate2::write::GzEncoder::new(BufWriter::new(f), flate2::Compression::fast());
        gz.write_all(bytes)?;
        gz.finish()?.flush()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn read_meta(path: &Path) -> Result<Meta> {
    let f = std::fs::File::open(path)?;
    let mut r = flate2::read::GzDecoder::new(BufReader::new(f));
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        bail!("not a SizeMatters scan file");
    }
    let taken = r_u64(&mut r)? as i64;
    let label = r_str(&mut r)?;
    let _root = r_str(&mut r)?;
    let mut b2 = [0u8; 2];
    r.read_exact(&mut b2)?;
    let _total = r_u64(&mut r)?;
    let _free = r_u64(&mut r)?;
    let files = r_u64(&mut r)?;
    let alloc = r_u64(&mut r)?;
    Ok(Meta { taken, label, files, alloc })
}

pub fn load(path: &Path) -> Result<(Tree, Meta)> {
    let f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut r = flate2::read::GzDecoder::new(BufReader::new(f));
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        bail!("not a SizeMatters scan file");
    }
    let taken = r_u64(&mut r)? as i64;
    let label = r_str(&mut r)?;
    let root = r_str(&mut r)?;
    let mut b2 = [0u8; 2];
    r.read_exact(&mut b2)?;
    let method = match b2[0] {
        0 => ScanMethod::Mft,
        2 => ScanMethod::Ssh,
        _ => ScanMethod::Walk,
    };
    let sep = b2[1] as char;
    let total = r_u64(&mut r)?;
    let free = r_u64(&mut r)?;
    let files = r_u64(&mut r)?;
    let alloc = r_u64(&mut r)?;
    let n = r_u32(&mut r)?;
    let mut entries = Vec::with_capacity(n as usize);
    for id in 0..n {
        let parent = r_u32(&mut r)?;
        let fl = r_u16(&mut r)?;
        let size = r_u64(&mut r)?;
        let al = r_u64(&mut r)?;
        let created = r_u64(&mut r)? as i64;
        let modified = r_u64(&mut r)? as i64;
        let accessed = r_u64(&mut r)? as i64;
        let name = r_str(&mut r)?;
        entries.push(RawEntry { id, parent, name, flags: fl, size, alloc: al, created, modified, accessed });
    }
    let mut t = build_tree(entries, n, 0, &root, method);
    t.sep = sep;
    t.volume_total = total;
    t.volume_free = free;
    Ok((t, Meta { taken, label, files, alloc }))
}

fn sanitize(label: &str) -> String {
    let s: String = label.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    s.trim_matches('_').chars().take(80).collect::<String>()
}

/// Automatic history: save this scan and keep the newest `keep` per location.
pub fn autosave(bytes: Vec<u8>, label: String, keep: usize) {
    std::thread::spawn(move || {
        let dir = data_dir().join("scans");
        let key = sanitize(&label);
        let file = dir.join(format!("{}_{}.smscan", if key.is_empty() { "root".into() } else { key.clone() }, chrono::Local::now().format("%Y%m%d-%H%M%S")));
        let _ = write_compressed(&bytes, &file);
        let mut mine: Vec<PathBuf> = history_files(&label);
        mine.sort();
        while mine.len() > keep {
            let _ = std::fs::remove_file(mine.remove(0));
        }
    });
}

fn history_files(label: &str) -> Vec<PathBuf> {
    let key = sanitize(label);
    let prefix = format!("{}_", if key.is_empty() { "root".into() } else { key });
    std::fs::read_dir(data_dir().join("scans"))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "smscan") && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&prefix)))
                .collect()
        })
        .unwrap_or_default()
}

/// Earlier scans of the same location, newest first (excluding ones newer than `before`).
pub fn history(label: &str) -> Vec<(PathBuf, Meta)> {
    let mut v: Vec<(PathBuf, Meta)> = history_files(label).into_iter().filter_map(|p| read_meta(&p).ok().map(|m| (p, m))).filter(|(_, m)| m.label == label).collect();
    v.sort_by(|a, b| b.1.taken.cmp(&a.1.taken));
    v
}

// ---------------------------------------------------------------------------
// What grew
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Change {
    /// Node in the current tree (None if it was removed).
    pub now: Option<u32>,
    pub path: String,
    pub before: u64,
    pub after: u64,
    pub is_dir: bool,
}

impl Change {
    pub fn delta(&self) -> i64 {
        self.after as i64 - self.before as i64
    }
}

/// Compare two scans of the same place by name, pruning unchanged subtrees.
pub fn grew(old: &Tree, new: &Tree, min_delta: u64, limit: usize) -> Vec<Change> {
    let mut out = Vec::new();
    let mut stack = vec![(old.root, new.root)];
    let metric = |t: &Tree, i: u32| t.node(i).alloc;
    while let Some((o, n)) = stack.pop() {
        let old_kids: HashMap<&str, u32> = old.children(o).map(|c| (old.name(c), c)).collect();
        let mut seen = std::collections::HashSet::new();
        for c in new.children(n) {
            let name = new.name(c);
            let after = metric(new, c);
            match old_kids.get(name) {
                Some(&oc) => {
                    seen.insert(name);
                    let before = metric(old, oc);
                    if before.abs_diff(after) >= min_delta {
                        out.push(Change { now: Some(c), path: new.path(c), before, after, is_dir: new.node(c).is_dir() });
                        if new.node(c).is_dir() && old.node(oc).is_dir() {
                            stack.push((oc, c));
                        }
                    }
                }
                None => {
                    if after >= min_delta {
                        out.push(Change { now: Some(c), path: new.path(c), before: 0, after, is_dir: new.node(c).is_dir() });
                    }
                }
            }
        }
        for (name, &oc) in &old_kids {
            if !seen.contains(name) && metric(old, oc) >= min_delta {
                out.push(Change { now: None, path: old.path(oc), before: metric(old, oc), after: 0, is_dir: old.node(oc).is_dir() });
            }
        }
    }
    out.sort_by(|a, b| b.delta().abs().cmp(&a.delta().abs()));
    out.truncate(limit);
    let _ = NONE;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RawEntry;

    fn tree(files: &[(&str, u64)]) -> Tree {
        let mut v = vec![RawEntry { id: 0, parent: 0, flags: flags::DIR, ..Default::default() }];
        let mut dirs: HashMap<String, u32> = HashMap::from([(String::new(), 0)]);
        for (path, size) in files {
            let parts: Vec<&str> = path.split('/').collect();
            let mut cur = String::new();
            for (k, p) in parts.iter().enumerate() {
                let parent = dirs[&cur];
                let next = if cur.is_empty() { p.to_string() } else { format!("{cur}/{p}") };
                let is_file = k + 1 == parts.len();
                if is_file {
                    let id = v.len() as u32;
                    v.push(RawEntry { id, parent, name: p.to_string(), size: *size, alloc: *size, ..Default::default() });
                } else if !dirs.contains_key(&next) {
                    let id = v.len() as u32;
                    v.push(RawEntry { id, parent, name: p.to_string(), flags: flags::DIR, ..Default::default() });
                    dirs.insert(next.clone(), id);
                }
                cur = next;
            }
        }
        let n = v.len() as u32;
        build_tree(v, n, 0, "/r", ScanMethod::Walk)
    }

    #[test]
    fn roundtrip_and_growth() {
        let old = tree(&[("a/x.bin", 100), ("a/y.bin", 50), ("b/z.bin", 500), ("gone.iso", 900)]);
        let new = tree(&[("a/x.bin", 1100), ("a/y.bin", 50), ("b/z.bin", 500), ("c/new.mkv", 700)]);
        let bytes = encode(&old);
        let p = std::env::temp_dir().join(format!("sm_snap_{}.smscan", std::process::id()));
        write_compressed(&bytes, &p).unwrap();
        let (loaded, meta) = load(&p).unwrap();
        assert_eq!(loaded.node(loaded.root).alloc, 1550);
        assert_eq!(meta.files, 4);
        let ch = grew(&loaded, &new, 10, 100);
        let find = |s: &str| ch.iter().find(|c| c.path.ends_with(s)).unwrap_or_else(|| panic!("{s} missing in {ch:?}"));
        assert_eq!(find("/a").delta(), 1000);
        assert_eq!(find("x.bin").delta(), 1000);
        assert_eq!(find("gone.iso").after, 0);
        assert_eq!(find("/c").before, 0);
        assert!(!ch.iter().any(|c| c.path.ends_with("/b")), "unchanged folders are pruned");
        let _ = std::fs::remove_file(p);
    }
}
