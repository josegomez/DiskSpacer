//! Headless mode: `sizematters --cli <drive-or-folder|ntfs-image> [--top N]`
//! Prints scan timing and the largest folders/files. Useful for benchmarking.

use crate::model::{ScanMethod, build_tree};
use crate::scan::{self, mft};
use std::path::PathBuf;
use std::time::Instant;

pub fn run(args: &[String]) -> i32 {
    let Some(target) = args.first() else {
        eprintln!("usage: sizematters --cli <C: | folder | image.ntfs> [--top N]");
        return 2;
    };
    let top: usize = args.iter().position(|a| a == "--top").and_then(|p| args.get(p + 1)).and_then(|s| s.parse().ok()).unwrap_or(15);
    let path = PathBuf::from(target);
    let t0 = Instant::now();
    let progress = mft::Progress::default();
    let tree = if path.is_file() {
        // An NTFS image file: parse its MFT directly.
        let f = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        };
        match mft::scan_mft(f, &progress) {
            Ok((entries, ids, g)) => {
                eprintln!("geometry: {g:?}, raw entries {}", entries.len());
                build_tree(entries, ids, mft::ROOT_RECORD, "\\", ScanMethod::Mft)
            }
            Err(e) => {
                eprintln!("MFT parse failed: {e:#}");
                return 1;
            }
        }
    } else {
        let p = std::sync::Arc::new(progress);
        let ctx = egui::Context::default();
        let rx = scan::start(scan::ScanRequest { path: path.clone(), prefer_mft: true }, p, ctx);
        match rx.recv() {
            Ok(scan::ScanMsg::Done(t, note)) => {
                if let Some(n) = note {
                    eprintln!("note: {n}");
                }
                *t
            }
            Ok(scan::ScanMsg::Failed(e)) => {
                eprintln!("scan failed: {e}");
                return 1;
            }
            Err(_) => return 1,
        }
    };
    let el = t0.elapsed();
    let r = tree.node(tree.root);
    println!("method   : {}", tree.method.label());
    println!("time     : {:.3}s", el.as_secs_f64());
    println!("files    : {}", r.files);
    println!("folders  : {}", r.dirs);
    println!("size     : {} ({} bytes)", crate::util::fmt_size(r.size), r.size);
    println!("on disk  : {} ({} bytes)", crate::util::fmt_size(r.alloc), r.alloc);
    println!("\nlargest top-level items:");
    for c in tree.children(tree.root).take(top) {
        let n = tree.node(c);
        println!("  {:>10}  {:>10}  {:>7} files  {}", crate::util::fmt_size(n.alloc), crate::util::fmt_size(n.size), n.files, tree.path(c));
    }
    let mut files: Vec<u32> = (0..tree.nodes.len() as u32).filter(|&i| !tree.node(i).is_dir() && tree.node(i).parent != crate::model::NONE).collect();
    files.sort_by_key(|&i| std::cmp::Reverse(tree.node(i).alloc));
    println!("\nlargest files (on disk):");
    for &f in files.iter().take(top) {
        let n = tree.node(f);
        println!("  {:>10}  {:>10}  {}", crate::util::fmt_size(n.alloc), crate::util::fmt_size(n.size), tree.path(f));
    }
    0
}
