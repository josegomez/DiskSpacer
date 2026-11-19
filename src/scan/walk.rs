//! Parallel directory walker — the fallback for non‑NTFS volumes, folders, or when
//! the process is not elevated (raw volume access requires administrator rights).

use crate::model::{RawEntry, flags};
use crate::scan::mft::Progress;
use parking_lot::Mutex;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::SystemTime;

fn unix(t: std::io::Result<SystemTime>) -> i64 {
    match t {
        Ok(t) => match t.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(e) => -(e.duration().as_secs() as i64),
        },
        Err(_) => 0,
    }
}

struct Ctx<'a> {
    next_id: AtomicU32,
    out: Mutex<Vec<RawEntry>>,
    progress: &'a Progress,
    #[cfg_attr(not(unix), allow(dead_code))]
    seen_inodes: Mutex<HashSet<(u64, u64)>>,
    cluster: u64,
}

pub fn walk(root: &Path, progress: &Progress) -> (Vec<RawEntry>, u32) {
    let ctx = Ctx {
        next_id: AtomicU32::new(1),
        out: Mutex::new(Vec::with_capacity(1 << 16)),
        progress,
        seen_inodes: Mutex::new(HashSet::new()),
        cluster: 4096,
    };
    let meta = std::fs::symlink_metadata(root).ok();
    ctx.out.lock().push(RawEntry {
        id: 0,
        parent: 0,
        name: String::new(),
        flags: flags::DIR,
        modified: meta.as_ref().map(|m| unix(m.modified())).unwrap_or(0),
        created: meta.as_ref().map(|m| unix(m.created())).unwrap_or(0),
        accessed: meta.as_ref().map(|m| unix(m.accessed())).unwrap_or(0),
        ..Default::default()
    });
    rayon::scope(|s| visit(s, &ctx, root.to_path_buf(), 0));
    let n = ctx.next_id.load(Ordering::Relaxed);
    (ctx.out.into_inner(), n)
}

fn visit<'s>(s: &rayon::Scope<'s>, ctx: &'s Ctx<'s>, dir: PathBuf, dir_id: u32) {
    if ctx.progress.cancel.load(Ordering::Relaxed) {
        return;
    }
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    let mut local = Vec::new();
    let mut subdirs = Vec::new();
    for ent in rd.flatten() {
        let Ok(md) = ent.metadata() else { continue };
        let ft = md.file_type();
        let id = ctx.next_id.fetch_add(1, Ordering::Relaxed);
        let mut fl = 0u16;
        let is_link = ft.is_symlink() || is_reparse(&md);
        if is_link {
            fl |= flags::REPARSE;
        }
        let is_dir = md.is_dir() && !is_link;
        if is_dir || (ft.is_symlink() && md.is_dir()) {
            fl |= flags::DIR;
        }
        fl |= attr_flags(&md);
        let mut size = if is_dir || is_link { 0 } else { md.len() };
        let mut alloc = alloc_size(&md, size, ctx.cluster);
        if !is_dir && hardlink_seen(ctx, &md) {
            fl |= flags::HARDLINK;
            size = 0;
            alloc = 0;
        }
        local.push(RawEntry {
            id,
            parent: dir_id,
            name: ent.file_name().to_string_lossy().into_owned(),
            flags: fl,
            size,
            alloc,
            created: unix(md.created()),
            modified: unix(md.modified()),
            accessed: unix(md.accessed()),
        });
        if is_dir {
            subdirs.push((ent.path(), id));
        }
    }
    ctx.progress.done.fetch_add(local.len() as u64, Ordering::Relaxed);
    ctx.out.lock().extend(local);
    for (p, id) in subdirs {
        s.spawn(move |s| visit(s, ctx, p, id));
    }
}

#[cfg(windows)]
fn is_reparse(md: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    md.file_attributes() & 0x400 != 0
}
#[cfg(not(windows))]
fn is_reparse(_md: &std::fs::Metadata) -> bool {
    false
}

#[cfg(windows)]
fn attr_flags(md: &std::fs::Metadata) -> u16 {
    use std::os::windows::fs::MetadataExt;
    let a = md.file_attributes();
    let mut f = 0;
    if a & 0x1 != 0 {
        f |= flags::READONLY;
    }
    if a & 0x2 != 0 {
        f |= flags::HIDDEN;
    }
    if a & 0x4 != 0 {
        f |= flags::SYSTEM;
    }
    if a & 0x800 != 0 {
        f |= flags::COMPRESSED;
    }
    if a & 0x200 != 0 {
        f |= flags::SPARSE;
    }
    if a & (0x1000 | 0x40000 | 0x400000) != 0 {
        f |= flags::OFFLINE;
    }
    f
}
#[cfg(not(windows))]
fn attr_flags(md: &std::fs::Metadata) -> u16 {
    if md.permissions().readonly() { flags::READONLY } else { 0 }
}

#[cfg(unix)]
fn alloc_size(md: &std::fs::Metadata, _size: u64, _cluster: u64) -> u64 {
    use std::os::unix::fs::MetadataExt;
    if md.is_dir() { 0 } else { md.blocks() * 512 }
}
#[cfg(not(unix))]
fn alloc_size(md: &std::fs::Metadata, size: u64, cluster: u64) -> u64 {
    // Cloud placeholders occupy (almost) nothing locally.
    if attr_flags(md) & flags::OFFLINE != 0 {
        return 0;
    }
    size.div_ceil(cluster) * cluster
}

#[cfg(unix)]
fn hardlink_seen(ctx: &Ctx, md: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    if md.nlink() <= 1 {
        return false;
    }
    !ctx.seen_inodes.lock().insert((md.dev(), md.ino()))
}
#[cfg(not(unix))]
fn hardlink_seen(_ctx: &Ctx, _md: &std::fs::Metadata) -> bool {
    false
}
