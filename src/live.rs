//! Live updates from the NTFS change journal (USN journal), like Everything.
//!
//! The journal position is recorded right before the MFT is read; afterwards a
//! background thread reads new journal records every ~1.5 s, coalesces them per file,
//! looks up each changed file's current size by file ID and sends the batch to the UI,
//! which patches the tree in place — no rescan.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Debug)]
pub struct Update {
    pub record: u32,
    pub parent: u32,
    pub name: String,
    pub is_dir: bool,
    pub exists: bool,
    pub size: u64,
    pub alloc: u64,
    pub modified: i64,
    pub attrs: u32,
}

#[cfg_attr(not(windows), allow(dead_code))]
pub enum Event {
    Changes(Vec<Update>),
    /// The journal was reset or overflowed: a rescan is needed.
    Lost(String),
}

pub struct Watcher {
    stop: Arc<AtomicBool>,
    pub rx: crossbeam_channel::Receiver<Event>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_STANDARD_INFO, FileBasicInfo, FileIdType, FileStandardInfo, GetFileInformationByHandleEx, OpenFileById,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00f4;
    const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00bb;
    const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const ERROR_JOURNAL_ENTRY_DELETED: u32 = 1181;
    const ERROR_JOURNAL_DELETE_IN_PROGRESS: u32 = 1178;
    const ERROR_JOURNAL_NOT_ACTIVE: u32 = 1179;

    #[repr(C)]
    #[derive(Default)]
    struct UsnJournalDataV0 {
        usn_journal_id: u64,
        first_usn: i64,
        next_usn: i64,
        lowest_valid_usn: i64,
        max_usn: i64,
        maximum_size: u64,
        allocation_delta: u64,
    }

    #[repr(C)]
    struct ReadUsnJournalDataV0 {
        start_usn: i64,
        reason_mask: u32,
        return_only_on_close: u32,
        timeout: u64,
        bytes_to_wait_for: u64,
        usn_journal_id: u64,
    }

    fn open_volume(letter: char) -> Option<std::fs::File> {
        std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE).open(format!(r"\\.\{letter}:")).ok()
    }

    /// (journal id, next USN) of the volume, if the change journal is active.
    pub fn query(vol: &std::fs::File) -> Option<(u64, i64)> {
        let mut data = UsnJournalDataV0::default();
        let mut ret = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                vol.as_raw_handle() as HANDLE,
                FSCTL_QUERY_USN_JOURNAL,
                std::ptr::null(),
                0,
                &mut data as *mut _ as *mut _,
                std::mem::size_of::<UsnJournalDataV0>() as u32,
                &mut ret,
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some((data.usn_journal_id, data.next_usn))
    }

    pub fn query_letter(letter: char) -> Option<(u64, i64)> {
        query(&open_volume(letter)?)
    }

    struct Raw {
        parent: u64,
        name: String,
        attrs: u32,
        reasons: u32,
        last_reason: u32,
        frn: u64,
    }

    fn file_info(hint: HANDLE, frn: u64) -> Option<(u64, u64, bool, i64, u32)> {
        let desc = FILE_ID_DESCRIPTOR { dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32, Type: FileIdType, Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: frn as i64 } };
        unsafe {
            let h = OpenFileById(hint, &desc, FILE_READ_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, std::ptr::null(), FILE_FLAG_BACKUP_SEMANTICS);
            if h == INVALID_HANDLE_VALUE || h.is_null() {
                return None;
            }
            let mut std_info: FILE_STANDARD_INFO = std::mem::zeroed();
            let mut basic: FILE_BASIC_INFO = std::mem::zeroed();
            let ok1 = GetFileInformationByHandleEx(h, FileStandardInfo, &mut std_info as *mut _ as *mut _, std::mem::size_of::<FILE_STANDARD_INFO>() as u32);
            let ok2 = GetFileInformationByHandleEx(h, FileBasicInfo, &mut basic as *mut _ as *mut _, std::mem::size_of::<FILE_BASIC_INFO>() as u32);
            CloseHandle(h);
            if ok1 == 0 {
                return None;
            }
            let modified = if ok2 != 0 { crate::model::filetime_to_unix(basic.LastWriteTime as u64) } else { 0 };
            let attrs = if ok2 != 0 { basic.FileAttributes } else { 0 };
            Some((std_info.EndOfFile as u64, std_info.AllocationSize as u64, std_info.Directory, modified, attrs))
        }
    }

    pub fn start(letter: char, journal_id: u64, start_usn: i64, ctx: egui::Context) -> Option<Watcher> {
        let vol = open_volume(letter)?;
        // a handle on the volume's root folder is the "hint" for OpenFileById
        let root = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(format!(r"{letter}:\"))
            .ok()?;
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = crossbeam_channel::unbounded();
        let s2 = stop.clone();
        std::thread::Builder::new()
            .name("usn-watch".into())
            .spawn(move || {
                let mut usn = start_usn;
                let mut buf = vec![0u8; 1 << 20];
                while !s2.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    let mut changed: std::collections::HashMap<u32, Raw> = std::collections::HashMap::new();
                    // drain everything new
                    loop {
                        let req = ReadUsnJournalDataV0 { start_usn: usn, reason_mask: 0xFFFF_FFFF, return_only_on_close: 0, timeout: 0, bytes_to_wait_for: 0, usn_journal_id: journal_id };
                        let mut ret = 0u32;
                        let ok = unsafe {
                            DeviceIoControl(
                                vol.as_raw_handle() as HANDLE,
                                FSCTL_READ_USN_JOURNAL,
                                &req as *const _ as *const _,
                                std::mem::size_of::<ReadUsnJournalDataV0>() as u32,
                                buf.as_mut_ptr() as *mut _,
                                buf.len() as u32,
                                &mut ret,
                                std::ptr::null_mut(),
                            )
                        };
                        if ok == 0 {
                            let err = unsafe { GetLastError() };
                            if matches!(err, ERROR_JOURNAL_ENTRY_DELETED | ERROR_JOURNAL_DELETE_IN_PROGRESS | ERROR_JOURNAL_NOT_ACTIVE) {
                                let _ = tx.send(Event::Lost("the change journal was reset or overflowed".into()));
                                ctx.request_repaint();
                                return;
                            }
                            break;
                        }
                        let ret = ret as usize;
                        if ret < 8 {
                            break;
                        }
                        let next = i64::from_le_bytes(buf[0..8].try_into().unwrap());
                        let mut off = 8;
                        while off + 60 <= ret {
                            let rlen = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
                            if rlen == 0 || off + rlen > ret {
                                break;
                            }
                            let r = &buf[off..off + rlen];
                            let major = u16::from_le_bytes([r[4], r[5]]);
                            if major == 2 {
                                let frn = u64::from_le_bytes(r[8..16].try_into().unwrap());
                                let parent = u64::from_le_bytes(r[16..24].try_into().unwrap());
                                let reason = u32::from_le_bytes(r[40..44].try_into().unwrap());
                                let attrs = u32::from_le_bytes(r[52..56].try_into().unwrap());
                                let nlen = u16::from_le_bytes([r[56], r[57]]) as usize;
                                let noff = u16::from_le_bytes([r[58], r[59]]) as usize;
                                if noff + nlen <= rlen {
                                    let u: Vec<u16> = r[noff..noff + nlen].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                                    let name = String::from_utf16_lossy(&u);
                                    let rec = (frn & 0x0000_FFFF_FFFF_FFFF) as u32;
                                    let e = changed.entry(rec).or_insert(Raw { parent, name: name.clone(), attrs, reasons: 0, last_reason: 0, frn });
                                    e.parent = parent;
                                    e.name = name;
                                    e.attrs = attrs;
                                    e.reasons |= reason;
                                    e.last_reason = reason;
                                    e.frn = frn;
                                }
                            }
                            off += rlen;
                        }
                        if next == usn {
                            break;
                        }
                        usn = next;
                    }
                    if changed.is_empty() {
                        continue;
                    }
                    let hint = root.as_raw_handle() as HANDLE;
                    let ups: Vec<Update> = changed
                        .into_iter()
                        .map(|(record, r)| {
                            let is_dir_attr = r.attrs & 0x10 != 0;
                            let parent = (r.parent & 0x0000_FFFF_FFFF_FFFF) as u32;
                            if r.last_reason & USN_REASON_FILE_DELETE != 0 {
                                return Update { record, parent, name: r.name, is_dir: is_dir_attr, exists: false, size: 0, alloc: 0, modified: 0, attrs: r.attrs };
                            }
                            match file_info(hint, r.frn) {
                                Some((size, alloc, is_dir, modified, attrs)) => Update { record, parent, name: r.name, is_dir, exists: true, size, alloc, modified, attrs },
                                None => Update { record, parent, name: r.name, is_dir: is_dir_attr, exists: false, size: 0, alloc: 0, modified: 0, attrs: r.attrs },
                            }
                        })
                        .collect();
                    if tx.send(Event::Changes(ups)).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            })
            .ok()?;
        Some(Watcher { stop, rx })
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub fn query_letter(_letter: char) -> Option<(u64, i64)> {
        None
    }
    pub fn start(_letter: char, _journal_id: u64, _start_usn: i64, _ctx: egui::Context) -> Option<Watcher> {
        None
    }
}

pub use imp::{query_letter, start};

/// Patch the tree with a batch of journal changes. Returns how many were applied.
pub fn apply(t: &mut crate::model::Tree, mut ups: Vec<Update>) -> usize {
    use crate::model::{NONE, flags};
    // folders first so new files find their (new) parent folder
    ups.sort_by_key(|u| (!u.is_dir, !u.exists));
    let live = |t: &crate::model::Tree, rec: u32| -> Option<u32> {
        t.id_map.get(rec as usize).copied().filter(|&n| n != NONE && !t.node(n).is_deleted() && (n == t.root || t.node(n).parent != NONE))
    };
    let mut applied = 0;
    let mut pending = ups;
    for _pass in 0..4 {
        let mut retry = Vec::new();
        for u in pending {
            let node = live(t, u.record);
            if !u.exists {
                if let Some(n) = node
                    && n != t.root
                {
                    t.mark_deleted(n);
                    applied += 1;
                }
                continue;
            }
            let Some(parent) = live(t, u.parent) else {
                retry.push(u); // parent not known (yet) — maybe outside the scanned tree
                continue;
            };
            match node {
                None => {
                    let mut fl = if u.is_dir { flags::DIR } else { 0 };
                    if u.attrs & 0x2 != 0 {
                        fl |= flags::HIDDEN;
                    }
                    if u.attrs & 0x4 != 0 {
                        fl |= flags::SYSTEM;
                    }
                    if u.attrs & 0x400 != 0 {
                        fl |= flags::REPARSE;
                    }
                    t.add_node(parent, &u.name, fl, u.size, u.alloc, u.modified, Some(u.record));
                    applied += 1;
                }
                Some(n) => {
                    if n == t.root {
                        continue;
                    }
                    if t.node(n).parent != parent {
                        t.move_node(n, parent);
                    }
                    if t.name(n) != u.name {
                        t.rename(n, &u.name);
                    }
                    if !u.is_dir && t.node(n).size != u.size {
                        t.update_file(n, u.size, u.alloc, u.modified);
                    }
                    applied += 1;
                }
            }
        }
        if retry.is_empty() {
            break;
        }
        pending = retry;
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{RawEntry, ScanMethod, build_tree, flags};

    #[test]
    fn applies_journal_batches() {
        let e = |id, parent, name: &str, dir: bool, size| RawEntry { id, parent, name: name.into(), flags: if dir { flags::DIR } else { 0 }, size, alloc: size, ..Default::default() };
        let mut t = build_tree(vec![e(5, 5, "", true, 0), e(10, 5, "docs", true, 0), e(11, 10, "a.txt", false, 100)], 20, 5, "C:\\\\", ScanMethod::Mft);
        let up = |record, parent, name: &str, is_dir, exists, size| Update { record, parent, name: name.into(), is_dir, exists, size, alloc: size, modified: 1, attrs: 0 };
        // new folder with a file in it (file listed first on purpose), a.txt grows, outside-tree file ignored
        let n = apply(&mut t, vec![up(13, 12, "new.bin", false, true, 50), up(12, 5, "newdir", true, true, 0), up(11, 10, "a.txt", false, true, 300), up(99, 77, "elsewhere", false, true, 1)]);
        assert_eq!(n, 3);
        assert_eq!(t.node(t.root).size, 350);
        // rename + move a.txt into newdir, then delete docs
        apply(&mut t, vec![up(11, 12, "b.txt", false, true, 300), up(10, 5, "docs", true, false, 0)]);
        let b = t.id_map[11];
        assert_eq!(t.name(b), "b.txt");
        assert!(t.path(b).replace('/', "\\\\").ends_with("newdir\\\\b.txt"));
        assert_eq!(t.node(t.root).size, 350);
        assert_eq!(t.node(t.root).dirs, 1);
    }
}
