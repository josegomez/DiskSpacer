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
}

#[derive(Clone, Debug)]
pub struct ScanRequest {
    /// A drive root ("C:\") or any folder.
    pub path: PathBuf,
    /// Try the raw MFT reader first.
    pub prefer_mft: bool,
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
    let mut note = None;

    if req.prefer_mft
        && let Some(letter) = platform::drive_letter_root(&req.path)
    {
        match platform::open_volume(letter) {
            Ok(vol) => match mft::scan_mft(vol, progress) {
                Ok((entries, ids, _g)) => {
                    let mut t = build_tree(entries, ids, mft::ROOT_RECORD, &format!("{letter}:\\"), ScanMethod::Mft);
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
