pub mod mft;
pub mod walk;

use crate::model::{ScanMethod, Tree, build_tree};
use crate::platform;
use mft::Progress;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

pub enum ScanMsg {
    Done(Box<Tree>, Option<String>),
    Failed(String),
    /// First connection to an SSH host: the user must confirm its key fingerprint.
    NeedTrust(crate::remote::Target, String, PathBuf),
}

#[derive(Clone)]
pub struct ScanRequest {
    /// A drive root ("C:\") or any folder.
    pub path: PathBuf,
    /// Try the raw MFT reader first.
    pub prefer_mft: bool,
    /// Scan `path` on this SSH host instead of locally.
    pub remote: Option<crate::remote::Target>,
    /// Reuse an existing SSH connection (rescans).
    pub conn: Option<Arc<crate::remote::Conn>>,
}

/// Run a scan on a background thread.
pub fn start(req: ScanRequest, progress: Arc<Progress>, ctx: egui::Context) -> crossbeam_channel::Receiver<ScanMsg> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::Builder::new()
        .name("scan".into())
        .spawn(move || {
            let msg = run(&req, &progress);
            let _ = tx.send(msg);
            ctx.request_repaint();
        })
        .expect("spawn scan thread");
    rx
}

fn run(req: &ScanRequest, progress: &Progress) -> ScanMsg {
    let t0 = Instant::now();
    let root_name = req.path.to_string_lossy().into_owned();
    // Developer aid: `synthetic:1000000` builds an in-memory tree for UI profiling.
    if let Some(n) = root_name.strip_prefix("synthetic:").and_then(|n| n.parse::<u32>().ok()) {
        let (entries, ids) = synthetic(n);
        let mut t = build_tree(entries, ids, 0, "S:\\", ScanMethod::Mft);
        t.volume_total = 2 << 40;
        t.volume_free = 1 << 40;
        t.scan_ms = t0.elapsed().as_millis();
        return ScanMsg::Done(Box::new(t), None);
    }
    let mut note = None;

    if req.remote.is_some() || req.conn.is_some() {
        return run_remote(req, progress, t0);
    }

    if req.prefer_mft
        && let Some(letter) = platform::drive_letter_root(&req.path)
    {
        // change-journal position *before* reading the MFT, so nothing is missed
        let usn = crate::live::query_letter(letter);
        match platform::open_volume(letter) {
            Ok(vol) => match mft::scan_mft(vol, progress) {
                Ok((entries, ids, _g)) => {
                    let mut t = build_tree(entries, ids, mft::ROOT_RECORD, &format!("{letter}:\\"), ScanMethod::Mft);
                    t.usn = usn.map(|(id, next)| (letter, id, next));
                    finish(&mut t, &req.path, t0);
                    return ScanMsg::Done(Box::new(t), None);
                }
                Err(e) if e.to_string() == "cancelled" => return ScanMsg::Failed("Scan cancelled".into()),
                Err(e) => note = Some(format!("MFT read failed ({e}); used directory walk instead.")),
            },
            Err(e) => note = Some(format!("Fast MFT scan unavailable ({e}); used directory walk instead.")),
        }
        progress.done.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    if !req.path.is_dir() {
        return ScanMsg::Failed(format!("{} is not a folder", req.path.display()));
    }
    let (entries, ids) = walk::walk(&req.path, progress);
    if progress.cancel.load(std::sync::atomic::Ordering::Relaxed) {
        return ScanMsg::Failed("Scan cancelled".into());
    }
    let mut t = build_tree(entries, ids, 0, &root_name, ScanMethod::Walk);
    finish(&mut t, &req.path, t0);
    ScanMsg::Done(Box::new(t), note)
}

fn finish(t: &mut Tree, path: &std::path::Path, t0: Instant) {
    if let Some((total, free)) = platform::disk_space(path) {
        t.volume_total = total;
        t.volume_free = free;
    }
    t.scan_ms = t0.elapsed().as_millis();
}

/// Deterministic pseudo-random tree shaped like a real system drive: deep folders,
/// a few huge flat directories, many extensions.
fn synthetic(n: u32) -> (Vec<crate::model::RawEntry>, u32) {
    use crate::model::{RawEntry, flags};
    let mut rng: u64 = 0x9E3779B97F4A7C15;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let exts = ["dll", "exe", "js", "json", "png", "jpg", "mp4", "txt", "log", "xml", "pdb", "cab", "sys", "dat", "zip", "rs", "ts", "map", "mui", "ttf"];
    let mut v = vec![RawEntry { id: 0, parent: 0, flags: flags::DIR, ..Default::default() }];
    let mut dirs: Vec<(u32, u32)> = vec![(0, 0)]; // (id, depth)
    let mut id = 1u32;
    while id < n {
        let r = next();
        // pick a parent folder, biased to recent (deeper) ones
        let pi = if r % 4 == 0 { (r >> 8) as usize % dirs.len() } else { dirs.len() - 1 - ((r >> 8) as usize % dirs.len().min(64)) };
        let (pid, pd) = dirs[pi];
        if r % 9 == 0 && pd < 14 {
            v.push(RawEntry { id, parent: pid, name: format!("folder_{id}"), flags: flags::DIR, modified: 1_600_000_000 + (r % 150_000_000) as i64, ..Default::default() });
            dirs.push((id, pd + 1));
        } else if r % 997 == 0 {
            // a huge flat folder
            let big = id;
            v.push(RawEntry { id, parent: pid, name: format!("flat_{id}"), flags: flags::DIR, ..Default::default() });
            id += 1;
            for _ in 0..20_000.min(n.saturating_sub(id)) {
                let s = next() % 200_000;
                v.push(RawEntry { id, parent: big, name: format!("f{id}.dll"), size: s, alloc: s.div_ceil(4096) * 4096, modified: 1_500_000_000 + (s as i64 * 997) % 250_000_000, ..Default::default() });
                id += 1;
            }
            continue;
        } else {
            let s = match r % 100 { 0 => next() % 4_000_000_000, 1..=10 => next() % 50_000_000, _ => next() % 300_000 };
            let e = exts[(r >> 20) as usize % exts.len()];
            v.push(RawEntry { id, parent: pid, name: format!("file_{id}.{e}"), size: s, alloc: s.div_ceil(4096) * 4096, modified: 1_500_000_000 + (r % 280_000_000) as i64, created: 1_400_000_000, accessed: 1_700_000_000, flags: 0 });
        }
        id += 1;
    }
    (v, id)
}

fn run_remote(req: &ScanRequest, progress: &Progress, t0: Instant) -> ScanMsg {
    use crate::remote::{self, ConnectError};
    let conn = match &req.conn {
        Some(c) => c.clone(),
        None => match remote::Conn::connect(req.remote.as_ref().unwrap()) {
            Ok(c) => Arc::new(c),
            Err(ConnectError::UnknownHost { fingerprint }) => return ScanMsg::NeedTrust(req.remote.clone().unwrap(), fingerprint, req.path.clone()),
            Err(e) => return ScanMsg::Failed(e.to_string()),
        },
    };
    let root = req.path.to_string_lossy().replace('\\', "/");
    let rp = remote::Progress { entries: Default::default(), bytes: Default::default(), cancel: Default::default() };
    // mirror progress into the shared counter the UI watches
    let res = std::thread::scope(|s| {
        let h = s.spawn(|| remote::scan(&conn, &root, &rp));
        while !h.is_finished() {
            progress.done.store(rp.entries.load(std::sync::atomic::Ordering::Relaxed), std::sync::atomic::Ordering::Relaxed);
            if progress.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                rp.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        h.join().unwrap()
    });
    match res {
        Ok((entries, ids)) => {
            let mut t = build_tree(entries, ids, 0, &root, ScanMethod::Ssh);
            t.sep = '/';
            if let Some((total, free)) = conn.disk_space(&root) {
                t.volume_total = total;
                t.volume_free = free;
            }
            t.remote = Some(conn);
            t.scan_ms = t0.elapsed().as_millis();
            ScanMsg::Done(Box::new(t), None)
        }
        Err(e) => ScanMsg::Failed(format!("{e:#}")),
    }
}
