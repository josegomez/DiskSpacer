//! Remote Linux scanning over SSH.
//!
//! One `find -printf` command streams the whole listing (gzip-compressed on the remote
//! side when available) — no per-file round trips — and is parsed as it arrives.
//! Host keys are checked against `~/.ssh/known_hosts`; unknown hosts must be confirmed by
//! fingerprint, changed keys are refused.

use crate::model::{RawEntry, flags};
use anyhow::{Context, Result, anyhow, bail};
use russh::client;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Clone, Debug, Default)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<String>,
    pub key_path: Option<PathBuf>,
    pub key_pass: Option<String>,
    /// Fingerprint the user confirmed for an unknown host (added to known_hosts).
    pub trust_fingerprint: Option<String>,
}

impl Target {
    /// Parse `ssh://user@host[:port]/path` → (target, remote path).
    pub fn parse_url(url: &str) -> Option<(Target, String)> {
        let rest = url.strip_prefix("ssh://")?;
        let (auth, path) = match rest.find('/') {
            Some(p) => (&rest[..p], &rest[p..]),
            None => (rest, "/"),
        };
        let (user, hostport) = auth.split_once('@')?;
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().ok()?),
            None => (hostport, 22),
        };
        Some((Target { host: host.into(), port, user: user.into(), ..Default::default() }, path.to_string()))
    }

    pub fn label(&self) -> String {
        if self.port == 22 { format!("{}@{}", self.user, self.host) } else { format!("{}@{}:{}", self.user, self.host, self.port) }
    }
}

#[derive(Debug)]
pub enum ConnectError {
    UnknownHost { fingerprint: String },
    HostKeyChanged { fingerprint: String },
    Other(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::UnknownHost { fingerprint } => write!(f, "unknown host key {fingerprint}"),
            ConnectError::HostKeyChanged { fingerprint } => write!(
                f,
                "HOST KEY CHANGED ({fingerprint}). This can mean someone is intercepting the connection. Refusing to connect; fix ~/.ssh/known_hosts if the change is expected."
            ),
            ConnectError::Other(s) => f.write_str(s),
        }
    }
}

#[derive(Clone, Debug)]
enum KeyIssue {
    Unknown(String),
    Changed(String),
}

struct Handler {
    host: String,
    port: u16,
    trust: Option<String>,
    issue: Arc<parking_lot::Mutex<Option<KeyIssue>>>,
}

impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &russh::keys::PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            *self.issue.lock() = Some(KeyIssue::Unknown("(certificate)".into()));
            return Ok(false);
        };
        let fp = key.fingerprint(HashAlg::Sha256).to_string();
        match russh::keys::check_known_hosts(&self.host, self.port, key) {
            Ok(true) => Ok(true),
            Err(russh::keys::Error::KeyChanged { .. }) => {
                *self.issue.lock() = Some(KeyIssue::Changed(fp));
                Ok(false)
            }
            _ => {
                if self.trust.as_deref() == Some(fp.as_str()) {
                    let _ = russh::keys::known_hosts::learn_known_hosts(&self.host, self.port, key);
                    Ok(true)
                } else {
                    *self.issue.lock() = Some(KeyIssue::Unknown(fp));
                    Ok(false)
                }
            }
        }
    }
}

/// An authenticated SSH connection with its own small async runtime.
pub struct Conn {
    rt: tokio::runtime::Runtime,
    handle: client::Handle<Handler>,
    pub target: Target,
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

impl Conn {
    pub fn connect(t: &Target) -> std::result::Result<Conn, ConnectError> {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().map_err(|e| ConnectError::Other(e.to_string()))?;
        let issue = Arc::new(parking_lot::Mutex::new(None));
        let handler = Handler { host: t.host.clone(), port: t.port, trust: t.trust_fingerprint.clone(), issue: issue.clone() };
        let config = Arc::new(client::Config {
            inactivity_timeout: Some(std::time::Duration::from_secs(600)),
            keepalive_interval: Some(std::time::Duration::from_secs(30)),
            ..Default::default()
        });
        let res: std::result::Result<client::Handle<Handler>, ConnectError> = rt.block_on(async {
            let connect = client::connect(config, (t.host.as_str(), t.port), handler);
            let mut h = match tokio::time::timeout(std::time::Duration::from_secs(20), connect).await {
                Err(_) => return Err(ConnectError::Other(format!("timed out connecting to {}:{}", t.host, t.port))),
                Ok(Err(e)) => {
                    return Err(match issue.lock().clone() {
                        Some(KeyIssue::Unknown(fp)) => ConnectError::UnknownHost { fingerprint: fp },
                        Some(KeyIssue::Changed(fp)) => ConnectError::HostKeyChanged { fingerprint: fp },
                        None => ConnectError::Other(format!("connection failed: {e}")),
                    });
                }
                Ok(Ok(h)) => h,
            };
            // authentication: explicit key → password → default keys
            let mut ok = false;
            let mut last_err = String::from("no credentials given");
            if let Some(kp) = &t.key_path {
                match russh::keys::load_secret_key(kp, t.key_pass.as_deref()) {
                    Ok(key) => {
                        let hash = h.best_supported_rsa_hash().await.ok().flatten().flatten();
                        match h.authenticate_publickey(&t.user, PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await {
                            Ok(r) if r.success() => ok = true,
                            Ok(_) => last_err = "the server rejected the key".into(),
                            Err(e) => last_err = e.to_string(),
                        }
                    }
                    Err(e) => last_err = format!("can't load key {}: {e}", kp.display()),
                }
            }
            if !ok && let Some(pw) = &t.password {
                match h.authenticate_password(&t.user, pw).await {
                    Ok(r) if r.success() => ok = true,
                    Ok(_) => last_err = "wrong user name or password".into(),
                    Err(e) => last_err = e.to_string(),
                }
            }
            if !ok && t.key_path.is_none() {
                if let Some(home) = home_dir() {
                    for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
                        let p = home.join(".ssh").join(name);
                        if let Ok(key) = russh::keys::load_secret_key(&p, None) {
                            let hash = h.best_supported_rsa_hash().await.ok().flatten().flatten();
                            if let Ok(r) = h.authenticate_publickey(&t.user, PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await
                                && r.success()
                            {
                                ok = true;
                                break;
                            }
                        }
                    }
                }
            }
            if !ok {
                return Err(ConnectError::Other(format!("authentication failed: {last_err}")));
            }
            Ok(h)
        });
        Ok(Conn { rt, handle: res?, target: t.clone() })
    }

    /// Run a command, streaming stdout to `on_data`. Returns (exit code, stderr).
    pub fn exec(&self, cmd: &str, mut on_data: impl FnMut(&[u8]) -> bool) -> Result<(u32, String)> {
        self.rt.block_on(async {
            let mut ch = self.handle.channel_open_session().await.context("opening SSH channel")?;
            ch.exec(true, cmd).await.context("starting remote command")?;
            let mut code = 255;
            let mut stderr = Vec::new();
            while let Some(msg) = ch.wait().await {
                match msg {
                    russh::ChannelMsg::Data { data } => {
                        if !on_data(&data) {
                            let _ = ch.close().await;
                            break;
                        }
                    }
                    russh::ChannelMsg::ExtendedData { data, .. } => {
                        if stderr.len() < 64 * 1024 {
                            stderr.extend_from_slice(&data);
                        }
                    }
                    russh::ChannelMsg::ExitStatus { exit_status } => code = exit_status,
                    _ => {}
                }
            }
            Ok((code, String::from_utf8_lossy(&stderr).into_owned()))
        })
    }

    pub fn exec_string(&self, cmd: &str) -> Result<(u32, String, String)> {
        let mut out = Vec::new();
        let (code, err) = self.exec(cmd, |d| {
            out.extend_from_slice(d);
            true
        })?;
        Ok((code, String::from_utf8_lossy(&out).into_owned(), err))
    }

    /// (total, free) bytes of the filesystem holding `path`.
    pub fn disk_space(&self, path: &str) -> Option<(u64, u64)> {
        let (_, out, _) = self.exec_string(&format!("df -kP {} 2>/dev/null | tail -1", shell_quote(path))).ok()?;
        let f: Vec<&str> = out.split_whitespace().collect();
        let total = f.get(1)?.parse::<u64>().ok()? * 1024;
        let free = f.get(3)?.parse::<u64>().ok()? * 1024;
        Some((total, free))
    }

    /// Permanently delete paths (`rm -rf`, which never follows symlinks). Per-path results.
    pub fn delete(&self, paths: &[String]) -> Vec<Result<(), String>> {
        let mut results: Vec<Result<(), String>> = vec![Err("not attempted".into()); paths.len()];
        for (chunk_idx, chunk) in paths.chunks(100).enumerate() {
            let mut script = String::new();
            for (k, p) in chunk.iter().enumerate() {
                let q = shell_quote(p);
                script += &format!("rm -rf -- {q} 2>/dev/null; if [ -e {q} ] || [ -L {q} ]; then echo 'ERR {k}'; else echo 'OK {k}'; fi\n");
            }
            match self.exec_string(&script) {
                Ok((_, out, _)) => {
                    for line in out.lines() {
                        let mut it = line.split_whitespace();
                        if let (Some(st), Some(k)) = (it.next(), it.next().and_then(|k| k.parse::<usize>().ok()))
                            && let Some(slot) = results.get_mut(chunk_idx * 100 + k)
                        {
                            *slot = if st == "OK" { Ok(()) } else { Err("permission denied or in use".into()) };
                        }
                    }
                }
                Err(e) => {
                    for k in 0..chunk.len() {
                        results[chunk_idx * 100 + k] = Err(e.to_string());
                    }
                }
            }
        }
        results
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

pub struct Progress {
    pub entries: AtomicU64,
    pub bytes: AtomicU64,
    pub cancel: AtomicBool,
}

/// Incremental parser for the NUL-separated `find -printf` records.
struct Parser {
    buf: Vec<u8>,
    entries: Vec<RawEntry>,
    dirs: HashMap<String, u32>,
    inodes: HashSet<u64>,
    next_id: u32,
}

impl Parser {
    fn feed(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        let mut start = 0;
        while let Some(pos) = self.buf[start..].iter().position(|&b| b == 0) {
            let rec = self.buf[start..start + pos].to_vec();
            self.record(&rec);
            start += pos + 1;
        }
        self.buf.drain(..start);
    }

    fn record(&mut self, rec: &[u8]) {
        // type size blocks mtime atime ctime nlink inode relpath
        let mut f = rec.splitn(9, |&b| b == b'\t');
        let (Some(ty), Some(size), Some(blocks), Some(mt), Some(at), Some(ct), Some(nl), Some(ino), Some(rel)) =
            (f.next(), f.next(), f.next(), f.next(), f.next(), f.next(), f.next(), f.next(), f.next())
        else {
            return;
        };
        let num = |b: &[u8]| std::str::from_utf8(b).ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        let time = |b: &[u8]| std::str::from_utf8(b).ok().and_then(|s| s.split('.').next()).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
        let rel = String::from_utf8_lossy(rel).into_owned();
        let is_dir = ty == b"d";
        let is_link = ty == b"l";
        if rel.is_empty() {
            // the starting point itself
            if let Some(root) = self.entries.first_mut() {
                root.modified = time(mt);
            }
            return;
        }
        let (parent_rel, name) = match rel.rfind('/') {
            Some(p) => (&rel[..p], &rel[p + 1..]),
            None => ("", rel.as_str()),
        };
        let Some(&parent) = self.dirs.get(parent_rel) else { return };
        let id = self.next_id;
        self.next_id += 1;
        let mut fl = 0u16;
        if is_dir {
            fl |= flags::DIR;
        }
        if is_link {
            fl |= flags::REPARSE;
        }
        if name.starts_with('.') {
            fl |= flags::HIDDEN;
        }
        let (mut size, mut alloc) = if is_dir || is_link { (0, 0) } else { (num(size), num(blocks) * 512) };
        if !is_dir && num(nl) > 1 && !self.inodes.insert(num(ino)) {
            fl |= flags::HARDLINK;
            size = 0;
            alloc = 0;
        }
        self.entries.push(RawEntry { id, parent, name: name.to_string(), flags: fl, size, alloc, created: time(ct), modified: time(mt), accessed: time(at) });
        if is_dir {
            self.dirs.insert(rel, id);
        }
    }
}

/// List everything under `root` on the remote host (one filesystem, like a drive).
pub fn scan(conn: &Conn, root: &str, progress: &Progress) -> Result<(Vec<RawEntry>, u32)> {
    let q = shell_quote(root);
    let (code, out, _) = conn.exec_string(&format!("test -d {q} && echo yes; find --version 2>/dev/null | head -1"))?;
    if !out.contains("yes") {
        bail!("{root} is not a folder on the remote host (exit {code})");
    }
    if !out.contains("GNU findutils") {
        bail!("the remote host needs GNU find (findutils) for scanning");
    }
    let cmd = format!(
        "cd / && LC_ALL=C find {q} -xdev -printf '%y\\t%s\\t%b\\t%T@\\t%A@\\t%C@\\t%n\\t%i\\t%P\\0' 2>/dev/null | (gzip -1 2>/dev/null || cat)"
    );
    let mut parser = Parser {
        buf: Vec::new(),
        entries: vec![RawEntry { id: 0, parent: 0, flags: flags::DIR, ..Default::default() }],
        dirs: HashMap::from([(String::new(), 0)]),
        inodes: HashSet::new(),
        next_id: 1,
    };
    // Stream: detect gzip by its magic bytes, decompress incrementally.
    struct Sink<'a> {
        p: &'a mut Parser,
    }
    impl Write for Sink<'_> {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.p.feed(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut mode: Option<bool> = None; // Some(true) = gzip
    let mut head: Vec<u8> = Vec::new();
    let mut gz: Option<flate2::write::GzDecoder<Sink>> = None;
    let mut plain: Vec<u8> = Vec::new();
    let mut err: Option<String> = None;
    let parser_ptr: *mut Parser = &mut parser;
    let (status, stderr) = conn.exec(&cmd, |data| {
        if progress.cancel.load(Ordering::Relaxed) {
            return false;
        }
        progress.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
        if mode.is_none() {
            head.extend_from_slice(data);
            if head.len() < 2 {
                return true;
            }
            mode = Some(head[0] == 0x1f && head[1] == 0x8b);
            let first = std::mem::take(&mut head);
            if mode == Some(true) {
                // SAFETY: the parser outlives the decoder; both live only within this call.
                gz = Some(flate2::write::GzDecoder::new(Sink { p: unsafe { &mut *parser_ptr } }));
                if let Err(e) = gz.as_mut().unwrap().write_all(&first) {
                    err = Some(e.to_string());
                    return false;
                }
            } else {
                plain.extend_from_slice(&first);
            }
        } else if mode == Some(true) {
            if let Err(e) = gz.as_mut().unwrap().write_all(data) {
                err = Some(e.to_string());
                return false;
            }
        } else {
            plain.extend_from_slice(data);
        }
        if mode == Some(false) && plain.len() > 1 << 20 {
            unsafe { &mut *parser_ptr }.feed(&std::mem::take(&mut plain));
        }
        progress.entries.store(unsafe { &*parser_ptr }.entries.len() as u64, Ordering::Relaxed);
        true
    })?;
    if let Some(mut g) = gz.take() {
        let _ = g.try_finish();
    }
    if !plain.is_empty() {
        parser.feed(&plain);
    }
    if progress.cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }
    if let Some(e) = err {
        return Err(anyhow!("decompressing listing failed: {e}"));
    }
    if parser.entries.len() <= 1 && status != 0 {
        bail!("remote scan failed (exit {status}): {}", stderr.trim());
    }
    let n = parser.next_id;
    Ok((parser.entries, n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_find_records() {
        let mut p = Parser {
            buf: Vec::new(),
            entries: vec![RawEntry { id: 0, parent: 0, flags: flags::DIR, ..Default::default() }],
            dirs: HashMap::from([(String::new(), 0)]),
            inodes: HashSet::new(),
            next_id: 1,
        };
        let data = b"d\t4096\t8\t1700000000.5\t1\t1\t3\t10\t\0d\t4096\t8\t1700000000.0\t1\t1\t2\t11\tlogs\0f\t1000\t8\t1600000000.0\t1\t1\t1\t12\tlogs/a.log\0f\t500\t8\t1600000000.0\t1\t1\t2\t13\tlogs/x\twith tab\0f\t500\t8\t1600000000.0\t1\t1\t2\t13\tlink\0";
        // feed in awkward pieces
        for chunk in data.chunks(7) {
            p.feed(chunk);
        }
        assert_eq!(p.entries.len(), 5);
        let a = p.entries.iter().find(|e| e.name == "a.log").unwrap();
        assert_eq!((a.size, a.alloc, a.modified), (1000, 4096, 1_600_000_000));
        assert_eq!(a.parent, p.dirs["logs"]);
        assert!(p.entries.iter().any(|e| e.name == "x\twith tab"));
        let link = p.entries.iter().find(|e| e.name == "link").unwrap();
        assert!(link.flags & flags::HARDLINK != 0 && link.size == 0);
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// Runs against a real sshd when SM_SSH_TEST="user@host:port:keyfile" is set.
    #[test]
    fn live_scan_and_delete() {
        let Ok(spec) = std::env::var("SM_SSH_TEST") else { return };
        let parts: Vec<&str> = spec.split(':').collect();
        let (user, host) = parts[0].split_once('@').unwrap();
        let t = Target { host: host.into(), port: parts[1].parse().unwrap(), user: user.into(), key_path: Some(parts[2].into()), ..Default::default() };
        let c = Conn::connect(&t).map_err(|e| e.to_string()).unwrap();
        let (_, home, _) = c.exec_string("printf %s \"$HOME\"").unwrap();
        let dir = format!("{home}/sm_del_test");
        c.exec_string(&format!("rm -rf {0}; mkdir -p {0}/sub && echo x > {0}/sub/f && echo y > {0}/\"we'ird name\" && ln -s / {0}/rootlink", shell_quote(&dir))).unwrap();
        let p = Progress { entries: Default::default(), bytes: Default::default(), cancel: Default::default() };
        let (entries, _) = scan(&c, &dir, &p).unwrap();
        assert_eq!(entries.len(), 5, "{entries:?}"); // root + sub + f + weird + link
        // deleting the symlink must not touch its target
        let res = c.delete(&[format!("{dir}/sub"), format!("{dir}/we'ird name"), format!("{dir}/rootlink"), format!("{dir}/missing")]);
        assert!(res[0].is_ok() && res[1].is_ok() && res[2].is_ok(), "{res:?}");
        assert!(res[3].is_ok(), "already absent counts as removed");
        let (_, ls, _) = c.exec_string(&format!("ls -A {} ; ls / | head -3", shell_quote(&dir))).unwrap();
        assert!(!ls.contains("sub") && ls.contains("bin") || ls.contains("usr"), "{ls}");
        c.exec_string(&format!("rm -rf {}", shell_quote(&dir))).unwrap();
    }
}
