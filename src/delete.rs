//! Safe bulk deletion: protected-path guard, top-most de-duplication, staleness checks,
//! Recycle Bin or permanent removal that never follows junctions/symlinks.

use crate::model::{Tree, flags};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    RecycleBin,
    Permanent,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct Item {
    pub node: u32,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_link: bool,
    /// Secondary hard link: listed with 0 bytes, so skip the size check.
    pub is_hardlink: bool,
    pub size: u64,
    pub alloc: u64,
    pub files: u32,
}

#[derive(Default)]
pub struct Plan {
    pub items: Vec<Item>,
    pub blocked: Vec<(PathBuf, String)>,
    pub total_alloc: u64,
    pub total_size: u64,
    pub total_files: u64,
    pub has_cloud: bool,
    pub has_hidden_system: bool,
}

const PROTECTED_DIRS: &[&str] = &[
    "windows",
    "program files",
    "program files (x86)",
    "programdata",
    "users",
    "system volume information",
    "$recycle.bin",
    "boot",
    "efi",
    "recovery",
    "$windows.~bt",
    "$windows.~ws",
];
const PROTECTED_ROOT_FILES: &[&str] = &["pagefile.sys", "hiberfil.sys", "swapfile.sys", "bootmgr", "bootnxt", "dumpstack.log"];

/// Why a path must not be deleted (None = allowed).
pub fn protected_reason(t: &Tree, node: u32, path: &Path) -> Option<String> {
    let n = t.node(node);
    if node == t.root {
        return Some("volume / scan root".into());
    }
    if n.flags & flags::METAFILE != 0 {
        return Some("NTFS system metadata".into());
    }
    let name = t.name(node);
    if name.contains('\u{FFFD}') {
        return Some("name contains characters that can't be represented safely".into());
    }
    if let Ok(exe) = std::env::current_exe()
        && (exe == path || exe.starts_with(path))
    {
        return Some("contains SizeMatters itself".into());
    }
    // Only enforce the Windows layout rules at the top of a drive.
    let ps = path.to_string_lossy().to_lowercase();
    let comps: Vec<String> = ps
        .split(['\\', '/'])
        .filter(|c| !c.is_empty() && !(c.len() == 2 && c.ends_with(':')))
        .map(str::to_string)
        .collect();
    let is_drive = path.to_string_lossy().as_bytes().get(1) == Some(&b':');
    if is_drive {
        if comps.len() == 1 {
            if PROTECTED_DIRS.contains(&comps[0].as_str()) {
                return Some(format!("protected system folder ({})", comps[0]));
            }
            if PROTECTED_ROOT_FILES.contains(&comps[0].as_str()) {
                return Some("Windows system file".into());
            }
        }
        if comps.len() == 2 && comps[0] == "users" {
            return Some("a whole user profile".into());
        }
        if comps.len() >= 2 && comps[0] == "windows" && (comps[1] == "winsxs" || comps[1] == "system32" || comps[1] == "syswow64") {
            return Some("Windows component store/system files (use Disk Cleanup or DISM instead)".into());
        }
        if comps.len() >= 2 && comps[0] == "windows" {
            return Some("inside the Windows folder".into());
        }
    } else if comps.is_empty() {
        return Some("filesystem root".into());
    } else if t.sep == '/' {
        // Linux / Unix layout (local or remote over SSH)
        const TOP: &[&str] = &["bin", "boot", "dev", "etc", "home", "lib", "lib32", "lib64", "libx32", "proc", "root", "run", "sbin", "srv", "sys", "usr", "var", "snap", "opt", "mnt", "media", "tmp"];
        const NEVER_INSIDE: &[&str] = &["bin", "boot", "dev", "etc", "lib", "lib32", "lib64", "libx32", "proc", "run", "sbin", "sys", "usr"];
        let first = comps[0].as_str();
        if comps.len() == 1 && TOP.contains(&first) {
            return Some(format!("system folder /{first}"));
        }
        if NEVER_INSIDE.contains(&first) {
            return Some(format!("inside system folder /{first}"));
        }
        if comps.len() == 2 && first == "home" {
            return Some("a whole home directory".into());
        }
        if comps.len() >= 2 && first == "var" && comps[1] == "lib" {
            return Some("/var/lib holds package and service state".into());
        }
    }
    None
}

/// Turn a selection into a safe, de-duplicated deletion plan (only top-most items).
pub fn plan(t: &Tree, selection: &[u32]) -> Plan {
    let mut sel: Vec<u32> = selection.iter().copied().filter(|&i| !t.node(i).is_deleted()).collect();
    sel.sort_unstable();
    sel.dedup();
    let set: std::collections::HashSet<u32> = sel.iter().copied().collect();
    let mut p = Plan::default();
    for &i in &sel {
        // skip if an ancestor is also selected
        let mut cur = t.node(i).parent;
        let mut covered = false;
        while cur != crate::model::NONE {
            if set.contains(&cur) {
                covered = true;
                break;
            }
            if cur == t.root {
                break;
            }
            cur = t.node(cur).parent;
        }
        if covered {
            continue;
        }
        let path = PathBuf::from(t.path(i));
        if let Some(r) = protected_reason(t, i, &path) {
            p.blocked.push((path, r));
            continue;
        }
        let n = t.node(i);
        if n.flags & flags::OFFLINE != 0 {
            p.has_cloud = true;
        }
        if n.flags & (flags::HIDDEN | flags::SYSTEM) == (flags::HIDDEN | flags::SYSTEM) {
            p.has_hidden_system = true;
        }
        p.total_alloc += n.alloc;
        p.total_size += n.size;
        p.total_files += n.files as u64;
        p.items.push(Item {
            node: i,
            path,
            is_dir: n.is_dir(),
            is_link: n.flags & flags::REPARSE != 0,
            is_hardlink: n.flags & flags::HARDLINK != 0,
            size: n.size,
            alloc: n.alloc,
            files: n.files,
        });
    }
    p.items.sort_by(|a, b| b.alloc.cmp(&a.alloc));
    p
}

pub struct Job {
    pub done: AtomicUsize,
    pub total: usize,
    pub cancel: AtomicBool,
    pub finished: AtomicBool,
    pub results: parking_lot::Mutex<Vec<(u32, Result<(), String>)>>,
}

pub fn execute(items: Vec<Item>, method: Method, ctx: egui::Context, remote: Option<Arc<crate::remote::Conn>>) -> Arc<Job> {
    let job = Arc::new(Job {
        done: AtomicUsize::new(0),
        total: items.len(),
        cancel: AtomicBool::new(false),
        finished: AtomicBool::new(false),
        results: parking_lot::Mutex::new(Vec::new()),
    });
    let j = job.clone();
    std::thread::Builder::new()
        .name("delete".into())
        .spawn(move || {
            match &remote {
                Some(conn) => {
                    // Remote host: permanent `rm -rf` over SSH (no Recycle Bin there).
                    let paths: Vec<String> = items.iter().map(|i| i.path.to_string_lossy().into_owned()).collect();
                    let res = conn.delete(&paths);
                    let mut r = j.results.lock();
                    for (it, res) in items.iter().zip(res) {
                        r.push((it.node, res));
                    }
                    j.done.store(items.len(), Ordering::Relaxed);
                }
                None => run(&items, method, &j, &ctx),
            }
            j.finished.store(true, Ordering::SeqCst);
            ctx.request_repaint();
        })
        .expect("spawn delete thread");
    job
}

fn run(items: &[Item], method: Method, job: &Job, ctx: &egui::Context) {
    // Recycle Bin: batch through the shell for speed, fall back to per-item for errors.
    let mut pending: Vec<&Item> = Vec::new();
    let flush = |batch: &mut Vec<&Item>| {
        if batch.is_empty() {
            return;
        }
        let paths: Vec<&Path> = batch.iter().map(|i| i.path.as_path()).collect();
        if trash::delete_all(&paths).is_ok() {
            let mut r = job.results.lock();
            for it in batch.iter() {
                // The shell may refuse silently; verify it's gone.
                let res = if std::fs::symlink_metadata(&it.path).is_ok() { Err("still present after recycling".into()) } else { Ok(()) };
                r.push((it.node, res));
            }
        } else {
            for it in batch.iter() {
                let res = trash::delete(&it.path).map_err(|e| e.to_string());
                job.results.lock().push((it.node, res));
            }
        }
        job.done.fetch_add(batch.len(), Ordering::Relaxed);
        ctx.request_repaint();
        batch.clear();
    };

    for it in items {
        if job.cancel.load(Ordering::Relaxed) {
            break;
        }
        if let Err(e) = still_matches(it) {
            job.results.lock().push((it.node, Err(e)));
            job.done.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        match method {
            Method::RecycleBin => {
                if it.path.as_os_str().len() >= 250 {
                    // Shell APIs are unreliable with long paths.
                    job.results.lock().push((it.node, Err("path too long for the Recycle Bin – use permanent delete".into())));
                    job.done.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                pending.push(it);
                if pending.len() >= 64 {
                    flush(&mut pending);
                }
            }
            Method::Permanent => {
                let r = remove_permanently(it);
                job.results.lock().push((it.node, r));
                job.done.fetch_add(1, Ordering::Relaxed);
                ctx.request_repaint();
            }
        }
    }
    flush(&mut pending);
}

/// Make sure the thing on disk is still what we scanned (no surprise targets).
fn still_matches(it: &Item) -> Result<(), String> {
    let md = std::fs::symlink_metadata(&it.path).map_err(|e| format!("no longer accessible: {e}"))?;
    let link = md.file_type().is_symlink() || is_reparse(&md);
    if link != it.is_link {
        return Err("changed since scan (link status differs)".into());
    }
    if !it.is_link && md.is_dir() != it.is_dir {
        return Err("changed since scan (file/folder type differs)".into());
    }
    if !it.is_dir && !it.is_link && !it.is_hardlink && md.len() != it.size {
        return Err("changed since scan (size differs) – rescan first".into());
    }
    Ok(())
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

fn clear_readonly(p: &Path) {
    if let Ok(md) = std::fs::symlink_metadata(p) {
        let mut perm = md.permissions();
        if perm.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            let _ = std::fs::set_permissions(p, perm);
        }
    }
}

fn remove_permanently(it: &Item) -> Result<(), String> {
    let p = &it.path;
    let attempt = || -> std::io::Result<()> {
        if it.is_link {
            // Remove the link itself, never its target.
            if it.is_dir || std::fs::symlink_metadata(p).map(|m| m.is_dir()).unwrap_or(false) {
                std::fs::remove_dir(p).or_else(|_| std::fs::remove_file(p))
            } else {
                std::fs::remove_file(p)
            }
        } else if it.is_dir {
            // std's remove_dir_all does not traverse symlinks/junctions.
            std::fs::remove_dir_all(p)
        } else {
            std::fs::remove_file(p)
        }
    };
    match attempt() {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            // Read-only attributes block deletes on Windows: clear and retry.
            if it.is_dir && !it.is_link {
                clear_readonly_tree(p);
            } else {
                clear_readonly(p);
            }
            attempt().map_err(|e| e.to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}

fn clear_readonly_tree(p: &Path) {
    clear_readonly(p);
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() && !ft.is_symlink() {
                clear_readonly_tree(&e.path());
            } else {
                clear_readonly(&e.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{RawEntry, ScanMethod, build_tree};

    #[test]
    fn plan_dedups_and_blocks() {
        let mk = |id, parent, name: &str, dir| RawEntry {
            id,
            parent,
            name: name.into(),
            flags: if dir { flags::DIR } else { 0 },
            size: 10,
            alloc: 10,
            ..Default::default()
        };
        let t = build_tree(
            vec![mk(0, 0, "", true), mk(1, 0, "Windows", true), mk(2, 0, "data", true), mk(3, 2, "a.txt", false), mk(4, 1, "x", false)],
            5,
            0,
            "C:\\",
            ScanMethod::Mft,
        );
        let find = |n: &str| (0..t.nodes.len() as u32).find(|&i| t.name(i) == n).unwrap();
        let p = plan(&t, &[find("data"), find("a.txt"), find("Windows"), find("x")]);
        assert_eq!(p.items.len(), 1);
        assert_eq!(p.items[0].path, PathBuf::from("C:\\data"));
        assert_eq!(p.blocked.len(), 1);
        let p2 = plan(&t, &[find("x")]);
        assert_eq!(p2.blocked.len(), 1);
    }

    #[test]
    fn permanent_delete_real_files() {
        let dir = std::env::temp_dir().join(format!("ds_test_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/f.txt"), b"hello").unwrap();
        let it = Item { node: 1, path: dir.clone(), is_dir: true, is_link: false, is_hardlink: false, size: 0, alloc: 0, files: 1 };
        still_matches(&it).unwrap();
        remove_permanently(&it).unwrap();
        assert!(!dir.exists());
    }
}
