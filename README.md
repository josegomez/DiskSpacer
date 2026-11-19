# DiskSpacer

A fast Windows disk space analyzer written in Rust. It reads the NTFS **Master File Table** directly, the same technique Everything and WizTree use, so a whole drive with millions of files is indexed in seconds instead of minutes. There is no directory walk.

![Strata view](docs/strata.png)

## Highlights

- **Instant MFT scan.** It opens the raw volume (`\\.\C:`), finds `$MFT` from the boot sector and streams every FILE record in large sequential reads, parsing them in parallel.
  - Sizes are correct for hard links (counted once), sparse, compressed and CompactOS/WOF files (alternate streams are counted), cloud placeholders, resident files and `$BadClus`.
  - Without administrator rights, or on non-NTFS drives, it falls back to a parallel folder walk. One click restarts it elevated.
- **Strata view (default).** The drive is one bar split into coloured blocks, one per folder, and each row splits the block above it.
  - Labels are readable and the zoom is animated. Hovering highlights the branch.
  - Small items are merged into a "N smaller" block, and free space is shown at the end of the drive bar.
  - You can colour by folder, by kind of file, or by age.
- **Storage bar** showing what kind of data fills the current folder (Videos, Images, Apps, Archives, …). Click a category to highlight it.
- **Treemap view** (WinDirStat-style cushion shading with SpaceSniffer-style folder headers) when you want maximum density.
- **Tree list** with % bars, sizes, file counts and dates, kept in sync with the chart. It has keyboard navigation.
- **Everything-style instant search** across the whole drive:
  `report`, `*.iso`, `ext:mp4;mkv size:>500mb`, `older:2y ext:zip`, `folder: node_modules`, `path:downloads newer:7d`, `regex:^IMG_\d+`
- **Clean up panel.** Select files in a folder and all its subfolders by:
  - name pattern (globs or regex) and exclusions
  - extension and size range
  - created, modified and accessed dates (absolute, or relative like `90d`, `1y`)
  - kind: files, folders or both

  Presets cover temp & logs, files over 1 GB, files not modified in a year, videos, installers, `node_modules`/build folders and empty files.
- **Bulk delete, done safely.**
  - **Selecting:** tick items in any list, delete whole folder trees in one go, and export lists to CSV.
  - **Recycle Bin:** the default, batched through the shell.
  - **Permanent delete:** needs an explicit acknowledgement, plus typing `DELETE` for large selections.
  - **Never touched:** protected locations (drive root, `Windows`, `Program Files`, `ProgramData`, whole user profiles, `pagefile.sys`, NTFS metadata, …).
  - **No link following:** junctions and symlinks are removed as links, never followed.
  - **Re-checked before deleting:** every item is checked again, so anything that changed since the scan is skipped. Read-only attributes are cleared automatically.
  - **Afterwards:** the view updates in place without a rescan, and you get a per-item failure report.

## Build

```bash
cargo build --release
```

Cross-compile the Windows exe from Linux:

```bash
rustup target add x86_64-pc-windows-gnu   # plus: apt install mingw-w64
cargo build --release --target x86_64-pc-windows-gnu
```

CI (`.github/workflows/build.yml`) builds and tests on Windows and uploads `diskspacer.exe` as an artifact.

## Usage

- Run `diskspacer.exe`, pick a drive and click **Scan**. Run it as administrator for the instant MFT scan.
- `diskspacer.exe D:\` starts scanning a drive or folder right away.
- `diskspacer.exe --cli C:` runs a headless scan and prints timings and the largest items (handy for benchmarking). It also accepts an NTFS image file.

| Shortcut | Action |
|---|---|
| Ctrl+F | Search |
| Enter / double-click | Zoom into folder |
| Backspace, Alt+↑, mouse back | Zoom out |
| ↑ ↓ ← → | Navigate the tree |
| Del / Shift+Del | Recycle / delete permanently |

## Layout

| Path | What |
|---|---|
| `src/scan/mft.rs` | Raw NTFS MFT reader (generic over `Read + Seek`, tested against a real NTFS image in `tests/fixtures`) |
| `src/scan/walk.rs` | Parallel directory-walk fallback |
| `src/model.rs` | Compact flat tree (CSR child arrays, string arena) |
| `src/filter.rs` | Search query language and filter engine |
| `src/delete.rs` | Deletion planning, protection rules, execution |
| `src/ui/` | egui app: strata, treemap, tables, panels |
