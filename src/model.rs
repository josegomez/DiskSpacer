//! Compact in-memory index of every file and folder on a volume.
//!
//! Nodes live in one flat `Vec`, names in one big string arena, and the
//! child lists in one CSR-style array (`children[child_start..child_start+child_count]`),
//! sorted by size descending. This keeps millions of entries cheap and cache friendly.

use std::collections::HashMap;

pub const NONE: u32 = u32::MAX;

pub mod flags {
    pub const DIR: u16 = 1 << 0;
    pub const HIDDEN: u16 = 1 << 1;
    pub const SYSTEM: u16 = 1 << 2;
    pub const READONLY: u16 = 1 << 3;
    pub const REPARSE: u16 = 1 << 4;
    pub const COMPRESSED: u16 = 1 << 5;
    pub const SPARSE: u16 = 1 << 6;
    /// Additional hard link: listed, but its bytes are counted at the primary link only.
    pub const HARDLINK: u16 = 1 << 7;
    /// Removed by the user during this session.
    pub const DELETED: u16 = 1 << 8;
    /// Cloud placeholder (OneDrive "online only" etc).
    pub const OFFLINE: u16 = 1 << 9;
    /// NTFS metadata file ($MFT, $LogFile, ...).
    pub const METAFILE: u16 = 1 << 10;
}

#[derive(Clone, Debug)]
pub struct Node {
    pub parent: u32,
    pub child_start: u32,
    pub child_count: u32,
    pub name_off: u32,
    pub name_len: u16,
    pub flags: u16,
    pub ext: u16,
    /// Logical size in bytes (for folders: sum of descendants).
    pub size: u64,
    /// Allocated size on disk (for folders: sum of descendants).
    pub alloc: u64,
    /// Number of files below (1 for a file).
    pub files: u32,
    /// Number of folders below (not counting itself).
    pub dirs: u32,
    /// Unix seconds. For folders, `modified` becomes the newest modification below.
    pub created: i64,
    pub modified: i64,
    pub accessed: i64,
}

impl Node {
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.flags & flags::DIR != 0
    }
    #[inline]
    pub fn is_deleted(&self) -> bool {
        self.flags & flags::DELETED != 0
    }
}

/// One entry produced by a scanner, before the tree is assembled.
#[derive(Clone, Debug, Default)]
pub struct RawEntry {
    /// Dense id of the file (MFT record number or walker index).
    pub id: u32,
    /// Dense id of the parent folder.
    pub parent: u32,
    pub name: String,
    pub flags: u16,
    pub size: u64,
    pub alloc: u64,
    pub created: i64,
    pub modified: i64,
    pub accessed: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanMethod {
    Mft,
    Walk,
}

impl ScanMethod {
    pub fn label(self) -> &'static str {
        match self {
            ScanMethod::Mft => "NTFS Master File Table (direct)",
            ScanMethod::Walk => "Directory walk",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ExtStat {
    pub name: String,
    pub size: u64,
    pub alloc: u64,
    pub count: u64,
}

pub struct Tree {
    pub nodes: Vec<Node>,
    pub names: String,
    pub children: Vec<u32>,
    pub exts: Vec<ExtStat>,
    pub root: u32,
    pub method: ScanMethod,
    pub volume_total: u64,
    pub volume_free: u64,
    pub scan_ms: u128,
}

impl Tree {
    #[inline]
    pub fn node(&self, i: u32) -> &Node {
        &self.nodes[i as usize]
    }

    #[inline]
    pub fn name(&self, i: u32) -> &str {
        let n = &self.nodes[i as usize];
        &self.names[n.name_off as usize..n.name_off as usize + n.name_len as usize]
    }

    /// Live (not deleted) children, largest first.
    pub fn children(&self, i: u32) -> impl Iterator<Item = u32> + '_ {
        let n = &self.nodes[i as usize];
        self.children[n.child_start as usize..(n.child_start + n.child_count) as usize]
            .iter()
            .copied()
            .filter(move |&c| !self.nodes[c as usize].is_deleted())
    }

    /// All child slots, including deleted ones.
    pub fn raw_children(&self, i: u32) -> &[u32] {
        let n = &self.nodes[i as usize];
        &self.children[n.child_start as usize..(n.child_start + n.child_count) as usize]
    }

    pub fn has_children(&self, i: u32) -> bool {
        self.children(i).next().is_some()
    }

    pub fn ext_name(&self, i: u32) -> &str {
        &self.exts[self.nodes[i as usize].ext as usize].name
    }

    pub fn path(&self, i: u32) -> String {
        let mut parts = Vec::new();
        let mut cur = i;
        while cur != NONE {
            parts.push(self.name(cur));
            if cur == self.root {
                break;
            }
            cur = self.nodes[cur as usize].parent;
        }
        parts.reverse();
        let mut s = String::new();
        for (k, p) in parts.iter().enumerate() {
            if k > 0 && !s.ends_with(['\\', '/']) {
                s.push(std::path::MAIN_SEPARATOR);
            }
            s.push_str(p);
        }
        s
    }

    /// Chain from root down to `i` (inclusive).
    pub fn ancestry(&self, i: u32) -> Vec<u32> {
        let mut v = Vec::new();
        let mut cur = i;
        while cur != NONE {
            v.push(cur);
            if cur == self.root {
                break;
            }
            cur = self.nodes[cur as usize].parent;
        }
        v.reverse();
        v
    }

    pub fn is_ancestor(&self, anc: u32, mut i: u32) -> bool {
        while i != NONE {
            if i == anc {
                return true;
            }
            if i == self.root {
                return false;
            }
            i = self.nodes[i as usize].parent;
        }
        false
    }

    /// Visit `start` and every live descendant (pre-order, iterative).
    pub fn for_each_descendant(&self, start: u32, mut f: impl FnMut(u32)) {
        let mut stack = vec![start];
        while let Some(i) = stack.pop() {
            f(i);
            stack.extend(self.children(i));
        }
    }

    /// Mark a node (and its subtree) deleted and subtract its totals from all ancestors.
    pub fn mark_deleted(&mut self, i: u32) {
        if self.nodes[i as usize].is_deleted() || i == self.root {
            return;
        }
        let (size, alloc, files, dirs, is_dir) = {
            let n = &self.nodes[i as usize];
            (n.size, n.alloc, n.files, n.dirs, n.is_dir())
        };
        self.nodes[i as usize].flags |= flags::DELETED;
        let ext = self.nodes[i as usize].ext as usize;
        if !is_dir {
            let e = &mut self.exts[ext];
            e.size = e.size.saturating_sub(size);
            e.alloc = e.alloc.saturating_sub(alloc);
            e.count = e.count.saturating_sub(1);
        } else {
            // flag the whole subtree and subtract extension stats of all files below
            let mut stack: Vec<u32> = self.raw_children(i).to_vec();
            while let Some(c) = stack.pop() {
                if self.nodes[c as usize].is_deleted() {
                    continue;
                }
                self.nodes[c as usize].flags |= flags::DELETED;
                let n = &self.nodes[c as usize];
                if n.is_dir() {
                    stack.extend_from_slice(self.raw_children(c));
                } else {
                    let (s, a, x) = (n.size, n.alloc, n.ext as usize);
                    let e = &mut self.exts[x];
                    e.size = e.size.saturating_sub(s);
                    e.alloc = e.alloc.saturating_sub(a);
                    e.count = e.count.saturating_sub(1);
                }
            }
        }
        let mut p = self.nodes[i as usize].parent;
        while p != NONE {
            let n = &mut self.nodes[p as usize];
            n.size = n.size.saturating_sub(size);
            n.alloc = n.alloc.saturating_sub(alloc);
            n.files = n.files.saturating_sub(files);
            n.dirs = n.dirs.saturating_sub(dirs + is_dir as u32);
            if p == self.root {
                break;
            }
            p = n.parent;
        }
        self.volume_free = self.volume_free.saturating_add(alloc);
        // keep sibling order sorted
        let parent = self.nodes[i as usize].parent;
        if parent != NONE {
            self.resort_upwards(parent);
        }
    }

    fn resort_upwards(&mut self, mut p: u32) {
        while p != NONE {
            let n = &self.nodes[p as usize];
            let (s, c) = (n.child_start as usize, n.child_count as usize);
            let nodes = &self.nodes;
            self.children[s..s + c].sort_unstable_by(|&a, &b| {
                nodes[b as usize].alloc.cmp(&nodes[a as usize].alloc)
            });
            if p == self.root {
                break;
            }
            p = self.nodes[p as usize].parent;
        }
    }

}

fn ext_of(name: &str, is_dir: bool) -> String {
    if is_dir {
        return String::new();
    }
    match name.rfind('.') {
        Some(p) if p > 0 && p + 1 < name.len() && name.len() - p <= 16 => {
            name[p + 1..].to_ascii_lowercase()
        }
        _ => String::new(),
    }
}

/// Assemble a [`Tree`] from raw scanner output.
///
/// * `ids` – one past the largest id used.
/// * `root_id` – id of the root folder; `root_name` is shown for it (e.g. `C:\`).
pub fn build_tree(
    mut entries: Vec<RawEntry>,
    ids: u32,
    root_id: u32,
    root_name: &str,
    method: ScanMethod,
) -> Tree {
    // id -> node index of the first entry with that id (folders never have hard links).
    let mut id_to_node = vec![NONE; ids as usize + 1];

    // make sure root exists and comes first
    if let Some(pos) = entries.iter().position(|e| e.id == root_id) {
        entries.swap(0, pos);
    } else {
        entries.insert(
            0,
            RawEntry {
                id: root_id,
                parent: root_id,
                name: String::new(),
                flags: flags::DIR,
                ..Default::default()
            },
        );
    }
    entries[0].name = root_name.to_string();
    entries[0].flags |= flags::DIR;

    let mut names = String::with_capacity(entries.iter().map(|e| e.name.len()).sum());
    let mut nodes: Vec<Node> = Vec::with_capacity(entries.len());
    let mut ext_map: HashMap<String, u16> = HashMap::new();
    let mut exts: Vec<ExtStat> = vec![ExtStat { name: String::new(), ..Default::default() }];
    ext_map.insert(String::new(), 0);
    let mut parent_ids = Vec::with_capacity(entries.len());

    for e in entries.iter() {
        let idx = nodes.len() as u32;
        if (e.id as usize) < id_to_node.len() && id_to_node[e.id as usize] == NONE {
            id_to_node[e.id as usize] = idx;
        }
        let is_dir = e.flags & flags::DIR != 0;
        let ext = ext_of(&e.name, is_dir);
        let ext_idx = match ext_map.get(&ext) {
            Some(&x) => x,
            None => {
                if exts.len() < u16::MAX as usize {
                    let x = exts.len() as u16;
                    exts.push(ExtStat { name: ext.clone(), ..Default::default() });
                    ext_map.insert(ext, x);
                    x
                } else {
                    0
                }
            }
        };
        let name_len = e.name.len().min(u16::MAX as usize);
        let off = names.len() as u32;
        names.push_str(&e.name[..floor_char_boundary(&e.name, name_len)]);
        nodes.push(Node {
            parent: NONE,
            child_start: 0,
            child_count: 0,
            name_off: off,
            name_len: (names.len() as u32 - off) as u16,
            flags: e.flags,
            ext: ext_idx,
            size: if is_dir { 0 } else { e.size },
            alloc: if is_dir { 0 } else { e.alloc },
            files: if is_dir { 0 } else { 1 },
            dirs: 0,
            created: e.created,
            modified: e.modified,
            accessed: e.accessed,
        });
        parent_ids.push(e.parent);
    }
    drop(entries);

    // Resolve parents. Entries whose parent is unknown or not a folder are dropped
    // (parent stays NONE and they are never linked in).
    let n = nodes.len();
    for i in 1..n {
        let pid = parent_ids[i] as usize;
        if pid < id_to_node.len() {
            let p = id_to_node[pid];
            if p != NONE && p as usize != i && nodes[p as usize].is_dir() {
                nodes[i].parent = p;
            }
        }
    }
    drop(parent_ids);
    drop(id_to_node);

    // Drop anything whose ancestor chain doesn't reach the root (orphans, cycles).
    // state: 0 unknown, 1 reaches root, 2 doesn't
    let mut state = vec![0u8; n];
    state[0] = 1;
    let mut chain = Vec::new();
    for i in 1..n {
        if state[i] != 0 {
            continue;
        }
        chain.clear();
        let mut cur = i;
        let result;
        loop {
            if state[cur] != 0 {
                result = state[cur];
                break;
            }
            if chain.len() > 4096 {
                result = 2;
                break;
            }
            state[cur] = 3; // visiting
            chain.push(cur);
            let p = nodes[cur].parent;
            if p == NONE {
                result = 2;
                break;
            }
            cur = p as usize;
            if state[cur] == 3 {
                result = 2;
                break;
            }
        }
        for &c in &chain {
            state[c] = result;
        }
    }
    for i in 1..n {
        if state[i] != 1 {
            nodes[i].parent = NONE;
        }
    }
    nodes[0].parent = NONE;

    // Children CSR
    let mut counts = vec![0u32; n];
    for i in 1..n {
        let p = nodes[i].parent;
        if p != NONE {
            counts[p as usize] += 1;
        }
    }
    let mut start = 0u32;
    for i in 0..n {
        nodes[i].child_start = start;
        start += counts[i];
    }
    let mut children = vec![0u32; start as usize];
    let mut fill = vec![0u32; n];
    for i in 1..n {
        let p = nodes[i].parent;
        if p != NONE {
            let pu = p as usize;
            children[(nodes[pu].child_start + fill[pu]) as usize] = i as u32;
            fill[pu] += 1;
        }
    }
    for i in 0..n {
        nodes[i].child_count = counts[i];
    }
    drop(counts);
    drop(fill);

    // Aggregate sizes bottom-up with an explicit post-order.
    let mut order = Vec::with_capacity(n);
    let mut stack = vec![0u32];
    while let Some(i) = stack.pop() {
        order.push(i);
        let nd = &nodes[i as usize];
        stack.extend_from_slice(
            &children[nd.child_start as usize..(nd.child_start + nd.child_count) as usize],
        );
    }
    for &i in order.iter().rev() {
        let nd = &nodes[i as usize];
        let p = nd.parent;
        if p == NONE {
            continue;
        }
        let (s, a, f, d, m, is_dir) = (nd.size, nd.alloc, nd.files, nd.dirs, nd.modified, nd.is_dir());
        let pn = &mut nodes[p as usize];
        pn.size += s;
        pn.alloc += a;
        pn.files += f;
        pn.dirs += d + is_dir as u32;
        if m > pn.modified {
            pn.modified = m;
        }
    }
    for &i in &order {
        let nd = &nodes[i as usize];
        if !nd.is_dir() {
            let e = &mut exts[nd.ext as usize];
            e.size += nd.size;
            e.alloc += nd.alloc;
            e.count += 1;
        }
    }

    // Sort children by allocated size descending.
    {
        let nodes_ref = &nodes;
        let mut slices: Vec<&mut [u32]> = Vec::new();
        let mut rest: &mut [u32] = &mut children;
        let mut consumed = 0u32;
        // nodes' child ranges are laid out in index order, contiguous.
        for nd in nodes_ref.iter() {
            let skip = nd.child_start - consumed;
            debug_assert_eq!(skip, 0);
            let (a, b) = rest.split_at_mut(nd.child_count as usize);
            consumed += nd.child_count;
            if nd.child_count > 1 {
                slices.push(a);
            }
            rest = b;
        }
        use rayon::prelude::*;
        slices.par_iter_mut().for_each(|s| {
            s.sort_unstable_by(|&a, &b| {
                let (na, nb) = (&nodes_ref[a as usize], &nodes_ref[b as usize]);
                nb.alloc.cmp(&na.alloc).then(nb.size.cmp(&na.size))
            })
        });
    }

    Tree {
        nodes,
        names,
        children,
        exts,
        root: 0,
        method,
        volume_total: 0,
        volume_free: 0,
        scan_ms: 0,
    }
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Convert a Windows FILETIME (100ns ticks since 1601) to Unix seconds.
#[inline]
pub fn filetime_to_unix(ft: u64) -> i64 {
    if ft == 0 {
        return 0;
    }
    (ft / 10_000_000) as i64 - 11_644_473_600
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: u32, parent: u32, name: &str, dir: bool, size: u64) -> RawEntry {
        RawEntry {
            id,
            parent,
            name: name.into(),
            flags: if dir { flags::DIR } else { 0 },
            size,
            alloc: size,
            ..Default::default()
        }
    }

    #[test]
    fn builds_and_aggregates() {
        let entries = vec![
            e(1, 0, "a", true, 0),
            e(2, 1, "x.txt", false, 10),
            e(3, 1, "y.LOG", false, 30),
            e(4, 0, "z.bin", false, 5),
            e(0, 0, "", true, 0),
            e(9, 77, "orphan", false, 1000),
        ];
        let mut t = build_tree(entries, 10, 0, "R:\\", ScanMethod::Walk);
        let r = t.root;
        assert_eq!(t.node(r).size, 45);
        assert_eq!(t.node(r).files, 3);
        assert_eq!(t.node(r).dirs, 1);
        let kids: Vec<_> = t.children(r).map(|c| t.name(c).to_string()).collect();
        assert_eq!(kids, vec!["a", "z.bin"]);
        let a = t.children(r).next().unwrap();
        let first = t.children(a).next().unwrap();
        assert_eq!(t.name(first), "y.LOG");
        assert_eq!(t.ext_name(first), "log");
        assert!(t.path(first).ends_with("y.LOG"));
        t.mark_deleted(first);
        assert_eq!(t.node(r).size, 15);
        assert_eq!(t.node(a).files, 1);
        t.mark_deleted(a);
        assert_eq!(t.node(r).size, 5);
        assert_eq!(t.node(r).dirs, 0);
        assert_eq!(t.children(r).count(), 1);
    }
}
