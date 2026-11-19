//! Direct NTFS Master File Table reader.
//!
//! Instead of walking directories (millions of syscalls), we open the raw volume,
//! locate `$MFT` from the boot sector and stream every FILE record in large
//! sequential reads — the same technique used by Everything and WizTree.
//! The parser is generic over `Read + Seek`, so it runs against `\\.\C:` on
//! Windows and against NTFS image files in tests.

use crate::model::{RawEntry, filetime_to_unix, flags};
use anyhow::{Context, Result, bail};
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_REPARSE_POINT: u32 = 0xC0;
const ATTR_END: u32 = 0xFFFF_FFFF;

const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
const FILE_ATTRIBUTE_SPARSE: u32 = 0x200;
const FILE_ATTRIBUTE_REPARSE: u32 = 0x400;
const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x800;
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x40000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x400000;

pub const ROOT_RECORD: u32 = 5;

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct Geometry {
    pub bytes_per_sector: u32,
    pub cluster_size: u64,
    pub record_size: usize,
    pub mft_lcn: u64,
    pub total_sectors: u64,
}

pub fn parse_boot(boot: &[u8]) -> Result<Geometry> {
    if boot.len() < 512 || &boot[3..11] != b"NTFS    " {
        bail!("not an NTFS volume");
    }
    let bps = u16::from_le_bytes([boot[0x0B], boot[0x0C]]) as u32;
    let spc_raw = boot[0x0D];
    let spc: u64 = if spc_raw > 0x80 { 1u64 << (256 - spc_raw as u32) } else { spc_raw as u64 };
    if !(256..=65536).contains(&bps) || !bps.is_power_of_two() || spc == 0 {
        bail!("invalid NTFS geometry");
    }
    let cluster_size = bps as u64 * spc;
    let total_sectors = u64::from_le_bytes(boot[0x28..0x30].try_into().unwrap());
    let mft_lcn = u64::from_le_bytes(boot[0x30..0x38].try_into().unwrap());
    let cpr = boot[0x40] as i8;
    let record_size: usize = if cpr < 0 {
        1usize << (-(cpr as i32)) as u32
    } else {
        (cpr as u64 * cluster_size) as usize
    };
    if !(512..=65536).contains(&record_size) || !record_size.is_power_of_two() {
        bail!("invalid MFT record size {record_size}");
    }
    Ok(Geometry { bytes_per_sector: bps, cluster_size, record_size, mft_lcn, total_sectors })
}

#[inline]
fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
#[inline]
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()))
}
#[inline]
fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    b.get(o..o + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap()))
}

/// Apply the update-sequence fixups in place. Returns false if the record is corrupt.
pub fn apply_fixup(rec: &mut [u8]) -> bool {
    if rec.len() < 48 || &rec[0..4] != b"FILE" {
        return false;
    }
    let (Some(usa_off), Some(usa_cnt)) = (u16_at(rec, 4), u16_at(rec, 6)) else {
        return false;
    };
    let (usa_off, usa_cnt) = (usa_off as usize, usa_cnt as usize);
    if usa_cnt < 2 || usa_off + usa_cnt * 2 > rec.len() || (usa_cnt - 1) * 512 > rec.len() {
        return false;
    }
    let usn = [rec[usa_off], rec[usa_off + 1]];
    for i in 1..usa_cnt {
        let end = i * 512 - 2;
        if rec[end] != usn[0] || rec[end + 1] != usn[1] {
            return false;
        }
        rec[end] = rec[usa_off + i * 2];
        rec[end + 1] = rec[usa_off + i * 2 + 1];
    }
    true
}

/// A run of clusters: (lcn, length). `lcn == None` is a sparse run.
pub type Run = (Option<u64>, u64);

pub fn decode_runs(mut b: &[u8]) -> Vec<Run> {
    let mut runs = Vec::new();
    let mut lcn: i64 = 0;
    while let Some(&hdr) = b.first() {
        if hdr == 0 {
            break;
        }
        let len_sz = (hdr & 0x0F) as usize;
        let off_sz = (hdr >> 4) as usize;
        if len_sz == 0 || len_sz > 8 || off_sz > 8 || b.len() < 1 + len_sz + off_sz {
            break;
        }
        let mut len: u64 = 0;
        for i in 0..len_sz {
            len |= (b[1 + i] as u64) << (8 * i);
        }
        if off_sz == 0 {
            runs.push((None, len));
        } else {
            let mut off: i64 = 0;
            for i in 0..off_sz {
                off |= (b[1 + len_sz + i] as i64) << (8 * i);
            }
            // sign extend
            let shift = 64 - 8 * off_sz as u32;
            off = (off << shift) >> shift;
            lcn += off;
            if lcn < 0 {
                break;
            }
            runs.push((Some(lcn as u64), len));
        }
        b = &b[1 + len_sz + off_sz..];
    }
    runs
}

/// Per-record accumulated facts (merged from base + extension records).
#[derive(Default, Clone)]
struct FileInfo {
    in_use: bool,
    is_dir: bool,
    seq: u16,
    attrs: u32,
    created: u64,
    modified: u64,
    accessed: u64,
    size: u64,
    alloc: u64,
    /// extra allocation from named streams ($DATA:name), e.g. WofCompressedData
    stream_alloc: u64,
    has_unnamed_data: bool,
    reparse: bool,
}

struct NameRec {
    record: u32,
    parent: u32,
    parent_seq: u16,
    namespace: u8,
    name: String,
}

struct Parsed {
    infos: Vec<(u32, FileInfo)>,
    names: Vec<NameRec>,
}

/// Parse one fixed-up record. Extension records report their base record number.
fn parse_record(rec: &[u8], recno: u32, cluster_size: u64, out_info: &mut FileInfo, names: &mut Vec<NameRec>) {
    let Some(flags_) = u16_at(rec, 0x16) else { return };
    let first = u16_at(rec, 0x14).unwrap_or(0) as usize;
    let used = (u32_at(rec, 0x18).unwrap_or(0) as usize).min(rec.len());
    let base = u64_at(rec, 0x20).unwrap_or(0) & 0x0000_FFFF_FFFF_FFFF;
    if base == 0 {
        out_info.in_use = flags_ & 1 != 0;
        out_info.is_dir = flags_ & 2 != 0;
        out_info.seq = u16_at(rec, 0x10).unwrap_or(0);
    }
    let owner = if base != 0 { base as u32 } else { recno };

    let mut off = first;
    while off + 16 <= used {
        let ty = u32_at(rec, off).unwrap_or(ATTR_END);
        if ty == ATTR_END {
            break;
        }
        let len = u32_at(rec, off + 4).unwrap_or(0) as usize;
        if len < 16 || off + len > used {
            break;
        }
        let a = &rec[off..off + len];
        let nonres = a[8] != 0;
        let name_len = a[9] as usize;
        let aflags = u16_at(a, 0x0C).unwrap_or(0);
        match ty {
            ATTR_STANDARD_INFORMATION if !nonres => {
                if let Some(v) = resident_value(a)
                    && v.len() >= 36
                {
                    out_info.created = u64_at(v, 0).unwrap();
                    out_info.modified = u64_at(v, 8).unwrap();
                    out_info.accessed = u64_at(v, 24).unwrap();
                    out_info.attrs = u32_at(v, 32).unwrap();
                }
            }
            ATTR_FILE_NAME if !nonres => {
                if let Some(v) = resident_value(a)
                    && v.len() >= 0x42
                {
                    let pref = u64_at(v, 0).unwrap();
                    let nlen = v[0x40] as usize;
                    let ns = v[0x41];
                    if v.len() >= 0x42 + nlen * 2 {
                        let u: Vec<u16> = (0..nlen).map(|i| u16_at(v, 0x42 + i * 2).unwrap()).collect();
                        names.push(NameRec {
                            record: owner,
                            parent: (pref & 0x0000_FFFF_FFFF_FFFF) as u32,
                            parent_seq: (pref >> 48) as u16,
                            namespace: ns,
                            name: String::from_utf16_lossy(&u),
                        });
                    }
                }
            }
            ATTR_DATA => {
                let named = name_len > 0;
                if nonres {
                    let start_vcn = u64_at(a, 0x10).unwrap_or(1);
                    if named {
                        // Named streams (incl. WofCompressedData): count the clusters actually
                        // mapped by this extent's runs. Sparse runs ($BadClus:$Bad) cost nothing.
                        let ro = u16_at(a, 0x20).unwrap_or(0) as usize;
                        if ro < a.len() {
                            let clusters: u64 = decode_runs(&a[ro..]).iter().filter(|r| r.0.is_some()).map(|r| r.1).sum();
                            out_info.stream_alloc += clusters * cluster_size;
                        }
                    } else if start_vcn == 0 {
                        let alloc = u64_at(a, 0x28).unwrap_or(0);
                        let real = u64_at(a, 0x30).unwrap_or(0);
                        let comp_unit = u16_at(a, 0x22).unwrap_or(0);
                        let on_disk = if (aflags & 0x8001 != 0 || comp_unit != 0) && len >= 0x48 {
                            u64_at(a, 0x40).unwrap_or(alloc)
                        } else {
                            alloc
                        };
                        out_info.size = real;
                        out_info.alloc = on_disk;
                        out_info.has_unnamed_data = true;
                    }
                } else if let Some(v) = resident_value(a) {
                    if !named {
                        out_info.size = v.len() as u64;
                        out_info.alloc = 0;
                        out_info.has_unnamed_data = true;
                    }
                }
            }
            ATTR_REPARSE_POINT => out_info.reparse = true,
            _ => {}
        }
        off += len;
    }
}

fn resident_value(a: &[u8]) -> Option<&[u8]> {
    let vlen = u32_at(a, 0x10)? as usize;
    let voff = u16_at(a, 0x14)? as usize;
    a.get(voff..voff.checked_add(vlen)?)
}

/// Read `len` bytes at `off`, rounding to sector alignment as raw volumes require.
fn read_aligned<R: Read + Seek>(r: &mut R, off: u64, len: usize, sector: u64, buf: &mut Vec<u8>) -> Result<()> {
    let a_off = off / sector * sector;
    let pre = (off - a_off) as usize;
    let a_len = (pre + len).div_ceil(sector as usize) * sector as usize;
    buf.resize(a_len, 0);
    r.seek(SeekFrom::Start(a_off))?;
    let mut got = 0;
    while got < a_len {
        let n = r.read(&mut buf[got..])?;
        if n == 0 {
            break;
        }
        got += n;
    }
    if got < pre + len {
        // short read (end of image) – zero the rest
        buf[got..].fill(0);
    }
    buf.drain(..pre);
    buf.truncate(len);
    Ok(())
}

/// Collect the data runs of `$MFT` itself (record 0), following $ATTRIBUTE_LIST if the
/// MFT is so fragmented that its runs spill into extension records.
fn mft_runs<R: Read + Seek>(r: &mut R, g: &Geometry) -> Result<(Vec<Run>, u64)> {
    let sector = g.bytes_per_sector as u64;
    let mut rec = Vec::new();
    read_aligned(r, g.mft_lcn * g.cluster_size, g.record_size, sector, &mut rec)?;
    if !apply_fixup(&mut rec) {
        bail!("$MFT record 0 is corrupt");
    }
    let mut extents: Vec<(u64, Vec<Run>)> = Vec::new();
    let mut data_size = 0u64;
    let mut ext_records: Vec<u64> = Vec::new();
    collect_mft_data(&rec, &mut extents, &mut data_size, Some(&mut ext_records), r, g)?;
    if !ext_records.is_empty() {
        // Need runs known so far to locate extension records.
        extents.sort_by_key(|e| e.0);
        let known: Vec<Run> = extents.iter().flat_map(|e| e.1.clone()).collect();
        for er in ext_records {
            if er == 0 {
                continue;
            }
            if let Some(off) = record_offset(&known, er, g) {
                let mut x = Vec::new();
                read_aligned(r, off, g.record_size, sector, &mut x)?;
                if apply_fixup(&mut x) {
                    collect_mft_data(&x, &mut extents, &mut data_size, None, r, g)?;
                }
            }
        }
    }
    extents.sort_by_key(|e| e.0);
    extents.dedup_by_key(|e| e.0);
    let runs = extents.into_iter().flat_map(|e| e.1).collect();
    Ok((runs, data_size))
}

fn record_offset(runs: &[Run], recno: u64, g: &Geometry) -> Option<u64> {
    let mut byte = recno * g.record_size as u64;
    for &(lcn, len) in runs {
        let run_bytes = len * g.cluster_size;
        if byte < run_bytes {
            return lcn.map(|l| l * g.cluster_size + byte);
        }
        byte -= run_bytes;
    }
    None
}

fn collect_mft_data<R: Read + Seek>(
    rec: &[u8],
    extents: &mut Vec<(u64, Vec<Run>)>,
    data_size: &mut u64,
    mut ext_records: Option<&mut Vec<u64>>,
    r: &mut R,
    g: &Geometry,
) -> Result<()> {
    let first = u16_at(rec, 0x14).unwrap_or(0) as usize;
    let used = (u32_at(rec, 0x18).unwrap_or(0) as usize).min(rec.len());
    let mut off = first;
    while off + 16 <= used {
        let ty = u32_at(rec, off).unwrap_or(ATTR_END);
        if ty == ATTR_END {
            break;
        }
        let len = u32_at(rec, off + 4).unwrap_or(0) as usize;
        if len < 16 || off + len > used {
            break;
        }
        let a = &rec[off..off + len];
        let nonres = a[8] != 0;
        if ty == ATTR_DATA && a[9] == 0 && nonres {
            let start_vcn = u64_at(a, 0x10).unwrap_or(0);
            if start_vcn == 0 {
                *data_size = u64_at(a, 0x30).unwrap_or(0);
            }
            let ro = u16_at(a, 0x20).unwrap_or(0) as usize;
            if ro < a.len() {
                extents.push((start_vcn, decode_runs(&a[ro..])));
            }
        } else if ty == ATTR_ATTRIBUTE_LIST
            && let Some(list) = ext_records.as_deref_mut()
        {
            let body: Vec<u8> = if nonres {
                let ro = u16_at(a, 0x20).unwrap_or(0) as usize;
                let size = u64_at(a, 0x30).unwrap_or(0) as usize;
                let runs = if ro < a.len() { decode_runs(&a[ro..]) } else { vec![] };
                let mut out = Vec::new();
                let mut tmp = Vec::new();
                for (lcn, l) in runs {
                    if let Some(lcn) = lcn {
                        read_aligned(r, lcn * g.cluster_size, (l * g.cluster_size) as usize, g.bytes_per_sector as u64, &mut tmp)?;
                        out.extend_from_slice(&tmp);
                    }
                }
                out.truncate(size);
                out
            } else {
                resident_value(a).map(|v| v.to_vec()).unwrap_or_default()
            };
            let mut p = 0;
            while p + 26 <= body.len() {
                let ety = u32_at(&body, p).unwrap();
                let elen = u16_at(&body, p + 4).unwrap() as usize;
                if elen == 0 {
                    break;
                }
                if ety == ATTR_DATA {
                    let mref = u64_at(&body, p + 16).unwrap() & 0x0000_FFFF_FFFF_FFFF;
                    if !list.contains(&mref) {
                        list.push(mref);
                    }
                }
                p += elen;
            }
        }
        off += len;
    }
    Ok(())
}

pub struct Progress {
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
}

impl Default for Progress {
    fn default() -> Self {
        Progress { done: AtomicU64::new(0), total: AtomicU64::new(0), cancel: AtomicBool::new(false) }
    }
}

/// Read and parse the whole MFT. Returns raw entries (one per hard link name) plus id bound.
pub fn scan_mft<R: Read + Seek + Send>(mut r: R, progress: &Progress) -> Result<(Vec<RawEntry>, u32, Geometry)> {
    let mut boot = Vec::new();
    read_aligned(&mut r, 0, 4096, 512, &mut boot).context("reading boot sector")?;
    let g = parse_boot(&boot)?;
    let (runs, data_size) = mft_runs(&mut r, &g)?;
    let rs = g.record_size as u64;
    let total_records = (data_size / rs).min(u32::MAX as u64 - 1) as u32;
    if total_records == 0 {
        bail!("empty MFT");
    }
    progress.total.store(total_records as u64, Ordering::Relaxed);

    // Reader thread streams chunks; this thread parses them in parallel.
    const CHUNK: u64 = 4 * 1024 * 1024;
    let (tx, rx) = crossbeam_channel::bounded::<(u32, Vec<u8>)>(4);
    let mut parsed_chunks: Vec<Parsed> = Vec::new();

    std::thread::scope(|s| -> Result<()> {
        let reader = s.spawn(move || -> Result<()> {
            let mut next_rec: u64 = 0;
            let total = total_records as u64;
            let sector = g.bytes_per_sector as u64;
            for (lcn, len) in runs {
                let run_bytes = len * g.cluster_size;
                let mut pos = 0u64;
                while pos < run_bytes && next_rec < total {
                    if progress.cancel.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    let remain_recs = total - next_rec;
                    let want = (run_bytes - pos).min(CHUNK).min(remain_recs * rs);
                    let want = want / rs * rs;
                    if want == 0 {
                        break;
                    }
                    let mut buf = Vec::new();
                    match lcn {
                        Some(l) => read_aligned(&mut r, l * g.cluster_size + pos, want as usize, sector, &mut buf)?,
                        None => buf.resize(want as usize, 0),
                    }
                    if tx.send((next_rec as u32, buf)).is_err() {
                        return Ok(());
                    }
                    next_rec += want / rs;
                    pos += want;
                }
            }
            Ok(())
        });

        use rayon::prelude::*;
        // Parse chunks as they arrive; each chunk's records are parsed in parallel.
        for (first, mut buf) in rx.iter() {
            let n = buf.len() / g.record_size;
            let parsed: Vec<(Vec<(u32, FileInfo)>, Vec<NameRec>)> = buf
                .par_chunks_mut(g.record_size)
                .enumerate()
                .fold(
                    || (Vec::new(), Vec::new()),
                    |(mut infos, mut names), (i, rec)| {
                        let recno = first + i as u32;
                        if apply_fixup(rec) && u16_at(rec, 0x16).unwrap_or(0) & 1 != 0 {
                            let mut fi = FileInfo::default();
                            parse_record(rec, recno, g.cluster_size, &mut fi, &mut names);
                            let base = (u64_at(rec, 0x20).unwrap_or(0) & 0x0000_FFFF_FFFF_FFFF) as u32;
                            infos.push((if base != 0 { base | 0x8000_0000 } else { recno }, fi));
                        }
                        (infos, names)
                    },
                )
                .collect();
            for (infos, names) in parsed {
                parsed_chunks.push(Parsed { infos, names });
            }
            progress.done.fetch_add(n as u64, Ordering::Relaxed);
        }
        reader.join().map_err(|_| anyhow::anyhow!("reader thread panicked"))??;
        Ok(())
    })?;

    if progress.cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }

    // Merge base and extension infos.
    let mut infos: Vec<FileInfo> = vec![FileInfo::default(); total_records as usize + 1];
    let mut ext_parts: Vec<(u32, FileInfo)> = Vec::new();
    let mut all_names: Vec<NameRec> = Vec::new();
    for p in parsed_chunks {
        for (key, fi) in p.infos {
            if key & 0x8000_0000 != 0 {
                ext_parts.push((key & 0x7FFF_FFFF, fi));
            } else if (key as usize) < infos.len() {
                infos[key as usize] = fi;
            }
        }
        all_names.extend(p.names);
    }
    for (base, fi) in ext_parts {
        if let Some(b) = infos.get_mut(base as usize) {
            if fi.has_unnamed_data && !b.has_unnamed_data {
                b.size = fi.size;
                b.alloc = fi.alloc;
                b.has_unnamed_data = true;
            }
            b.stream_alloc += fi.stream_alloc;
            b.reparse |= fi.reparse;
        }
    }

    // Choose names: per (record, parent) prefer Win32/POSIX names over DOS 8.3 names.
    all_names.sort_unstable_by(|a, b| {
        (a.record, a.parent, a.namespace == 2).cmp(&(b.record, b.parent, b.namespace == 2))
    });
    let mut entries = Vec::with_capacity(all_names.len());
    let mut last: Option<(u32, u32)> = None;
    let mut seen_record: u32 = u32::MAX;
    for nr in all_names {
        if nr.namespace == 2 && last == Some((nr.record, nr.parent)) {
            continue; // DOS 8.3 alias of a name we already have
        }
        last = Some((nr.record, nr.parent));
        let Some(fi) = infos.get(nr.record as usize) else { continue };
        if !fi.in_use {
            continue;
        }
        if let Some(p) = infos.get(nr.parent as usize) {
            if !p.in_use || (nr.record != ROOT_RECORD && p.seq != nr.parent_seq && nr.parent_seq != 0) {
                continue; // stale entry pointing at a reused/deleted folder
            }
        }
        let mut fl = 0u16;
        if fi.is_dir {
            fl |= flags::DIR;
        }
        if fi.attrs & FILE_ATTRIBUTE_HIDDEN != 0 {
            fl |= flags::HIDDEN;
        }
        if fi.attrs & FILE_ATTRIBUTE_SYSTEM != 0 {
            fl |= flags::SYSTEM;
        }
        if fi.attrs & FILE_ATTRIBUTE_READONLY != 0 {
            fl |= flags::READONLY;
        }
        if fi.reparse || fi.attrs & FILE_ATTRIBUTE_REPARSE != 0 {
            fl |= flags::REPARSE;
        }
        if fi.attrs & FILE_ATTRIBUTE_COMPRESSED != 0 {
            fl |= flags::COMPRESSED;
        }
        if fi.attrs & FILE_ATTRIBUTE_SPARSE != 0 {
            fl |= flags::SPARSE;
        }
        if fi.attrs & (FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS) != 0 {
            fl |= flags::OFFLINE;
        }
        if nr.record < 16 || (nr.parent == ROOT_RECORD && nr.name.starts_with('$') && nr.record < 64) {
            fl |= flags::METAFILE;
        }
        let first_link = seen_record != nr.record;
        seen_record = nr.record;
        let (size, alloc) = if fi.is_dir {
            (0, 0)
        } else if first_link {
            // $BadClus:$Bad is a sparse named stream spanning the volume – stream_alloc stays tiny.
            (fi.size, fi.alloc + fi.stream_alloc)
        } else {
            fl |= flags::HARDLINK;
            (0, 0)
        };
        entries.push(RawEntry {
            id: nr.record,
            parent: nr.parent,
            name: nr.name,
            flags: fl,
            size,
            alloc,
            created: filetime_to_unix(fi.created),
            modified: filetime_to_unix(fi.modified),
            accessed: filetime_to_unix(fi.accessed),
        });
    }
    Ok((entries, total_records + 1, g))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_decode() {
        // 0x21: 1-byte length, 2-byte offset. len=0x18, off=0x5634 ; then sparse run len 4 ; then negative offset
        let b = [0x21, 0x18, 0x34, 0x56, 0x01, 0x04, 0x11, 0x02, 0xFF, 0x00];
        let r = decode_runs(&b);
        assert_eq!(r, vec![(Some(0x5634), 0x18), (None, 4), (Some(0x5633), 2)]);
    }

    #[test]
    fn fixup_checks_tail() {
        let mut rec = vec![0u8; 1024];
        rec[0..4].copy_from_slice(b"FILE");
        rec[4] = 0x30; // usa offset
        rec[6] = 3; // usa count
        rec[0x30] = 0xAB;
        rec[0x31] = 0xCD;
        rec[0x32] = 1;
        rec[0x33] = 2;
        rec[0x34] = 3;
        rec[0x35] = 4;
        rec[510] = 0xAB;
        rec[511] = 0xCD;
        rec[1022] = 0xAB;
        rec[1023] = 0xCD;
        assert!(apply_fixup(&mut rec.clone()));
        let mut ok = rec.clone();
        apply_fixup(&mut ok);
        assert_eq!(&ok[510..512], &[1, 2]);
        assert_eq!(&ok[1022..1024], &[3, 4]);
        rec[1023] = 0;
        assert!(!apply_fixup(&mut rec));
    }
}
