//! OS-specific helpers: raw volume access, drive enumeration, elevation, shell integration.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct DriveInfo {
    pub root: PathBuf,
    pub label: String,
    pub fs: String,
    pub total: u64,
    pub free: u64,
}

/// If `p` is a drive root like `C:\` (or `C:`), return the letter.
pub fn drive_letter_root(p: &Path) -> Option<char> {
    let s = p.to_string_lossy();
    let s = s.trim_end_matches(['\\', '/']);
    let b = s.as_bytes();
    if b.len() == 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        Some(b[0].to_ascii_uppercase() as char)
    } else {
        None
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_READ, FILE_SHARE_WRITE, GetDiskFreeSpaceExW, GetLogicalDrives, GetVolumeInformationW,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    pub fn open_volume(letter: char) -> anyhow::Result<std::fs::File> {
        use std::os::windows::fs::OpenOptionsExt;
        let path = format!(r"\\.\{letter}:");
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&path)
            .map_err(|e| {
                if e.raw_os_error() == Some(5) {
                    anyhow::anyhow!("administrator rights required")
                } else {
                    anyhow::anyhow!("{e}")
                }
            })
    }

    pub fn disk_space(p: &Path) -> Option<(u64, u64)> {
        let w = wide(p.as_os_str());
        let (mut free_avail, mut total, mut free) = (0u64, 0u64, 0u64);
        let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut free_avail, &mut total, &mut free) };
        (ok != 0).then_some((total, free))
    }

    pub fn list_drives() -> Vec<DriveInfo> {
        let mask = unsafe { GetLogicalDrives() };
        let mut out = Vec::new();
        for i in 0..26 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            let root = PathBuf::from(format!("{letter}:\\"));
            let w = wide(root.as_os_str());
            let mut label = [0u16; 261];
            let mut fs = [0u16; 261];
            let ok = unsafe {
                GetVolumeInformationW(
                    w.as_ptr(),
                    label.as_mut_ptr(),
                    label.len() as u32,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    fs.as_mut_ptr(),
                    fs.len() as u32,
                )
            };
            if ok == 0 {
                continue; // empty card reader etc.
            }
            let z = |b: &[u16]| String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]);
            let (total, free) = disk_space(&root).unwrap_or((0, 0));
            out.push(DriveInfo { root, label: z(&label), fs: z(&fs), total, free });
        }
        out
    }

    pub fn is_elevated() -> bool {
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let mut elev = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut len = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                &mut elev as *mut _ as *mut _,
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            );
            CloseHandle(token);
            ok != 0 && elev.TokenIsElevated != 0
        }
    }

    pub fn relaunch_elevated(args: &str) -> bool {
        let Ok(exe) = std::env::current_exe() else { return false };
        let exe = wide(exe.as_os_str());
        let verb = wide(std::ffi::OsStr::new("runas"));
        let a = wide(std::ffi::OsStr::new(args));
        let r = unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), exe.as_ptr(), a.as_ptr(), std::ptr::null(), SW_SHOWNORMAL) };
        r as isize > 32
    }

    pub fn reveal(p: &Path) {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("explorer.exe").raw_arg(format!("/select,\"{}\"", p.display())).spawn();
    }

    pub fn open(p: &Path) {
        let w = wide(p.as_os_str());
        let verb = wide(std::ffi::OsStr::new("open"));
        unsafe {
            ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), w.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub fn open_volume(_letter: char) -> anyhow::Result<std::fs::File> {
        anyhow::bail!("raw volume access is only supported on Windows")
    }

    pub fn disk_space(p: &Path) -> Option<(u64, u64)> {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
            return None;
        }
        let f = s.f_frsize as u64;
        Some((s.f_blocks as u64 * f, s.f_bavail as u64 * f))
    }

    pub fn list_drives() -> Vec<DriveInfo> {
        let mut v = Vec::new();
        for p in ["/", std::env::var("HOME").unwrap_or_default().as_str()] {
            if p.is_empty() {
                continue;
            }
            let root = PathBuf::from(p);
            let (total, free) = disk_space(&root).unwrap_or((0, 0));
            v.push(DriveInfo { root, label: String::new(), fs: String::new(), total, free });
        }
        v
    }

    pub fn is_elevated() -> bool {
        unsafe { libc::geteuid() == 0 }
    }

    pub fn relaunch_elevated(_args: &str) -> bool {
        false
    }

    pub fn reveal(p: &Path) {
        let dir = if p.is_dir() { p } else { p.parent().unwrap_or(p) };
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }

    pub fn open(p: &Path) {
        let _ = std::process::Command::new("xdg-open").arg(p).spawn();
    }
}

pub use imp::*;
