//! File selection: the filter panel and the Everything‑style search box share this engine.
//!
//! Search syntax (space separated, all terms must match):
//!   `report`            name contains "report" (case-insensitive)
//!   `*.log` `a?c*`      wildcard on the name
//!   `ext:mp4;mkv`       extension list
//!   `size:>1gb` `size:<10k`
//!   `modified:<2023-01-01`  `created:>2024-06-01`  `accessed:<90d`
//!   `older:180d` `newer:7d`   (by modified date; d/w/m/y)
//!   `file:` `folder:`   restrict kind
//!   `path:temp`         full path contains text
//!   `regex:^IMG_\d+`    regular expression on the name

use crate::model::{NONE, Tree, flags};
use chrono::{Local, NaiveDate, TimeZone};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Kind {
    #[default]
    Files,
    Folders,
    Both,
}

/// The filter panel's editable state (strings, so the UI can bind directly).
#[derive(Clone, Debug)]
pub struct FilterSpec {
    pub name: String,
    pub use_regex: bool,
    pub case_sensitive: bool,
    pub exclude: String,
    pub exts: String,
    pub min_size: String,
    pub max_size: String,
    pub created_after: String,
    pub created_before: String,
    pub modified_after: String,
    pub modified_before: String,
    pub accessed_after: String,
    pub accessed_before: String,
    pub kind: Kind,
    pub include_hidden: bool,
    pub include_system: bool,
}

impl Default for FilterSpec {
    fn default() -> Self {
        FilterSpec {
            name: String::new(),
            use_regex: false,
            case_sensitive: false,
            exclude: String::new(),
            exts: String::new(),
            min_size: String::new(),
            max_size: String::new(),
            created_after: String::new(),
            created_before: String::new(),
            modified_after: String::new(),
            modified_before: String::new(),
            accessed_after: String::new(),
            accessed_before: String::new(),
            kind: Kind::Files,
            include_hidden: true,
            include_system: false,
        }
    }
}

enum NameMatch {
    Glob(GlobSet),
    Regex(Regex),
}

#[derive(Default)]
pub struct Compiled {
    name: Option<NameMatch>,
    exclude: Option<GlobSet>,
    substrings: Vec<String>,
    path_substrings: Vec<String>,
    exts: Vec<String>,
    min_size: Option<u64>,
    max_size: Option<u64>,
    created: (Option<i64>, Option<i64>),
    modified: (Option<i64>, Option<i64>),
    accessed: (Option<i64>, Option<i64>),
    kind: Kind,
    include_hidden: bool,
    include_system: bool,
    case_sensitive: bool,
    /// Measure size by allocated bytes rather than logical.
    pub use_alloc: bool,
}

pub fn parse_size(s: &str) -> Result<Option<u64>, String> {
    let s = s.trim().to_ascii_lowercase().replace(' ', "");
    if s.is_empty() {
        return Ok(None);
    }
    let idx = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(idx);
    let v: f64 = num.parse().map_err(|_| format!("bad size '{s}'"))?;
    let mul: f64 = match unit.trim_end_matches('b').trim_end_matches('i') {
        "" => 1.0,
        "k" => 1024.0,
        "m" => 1024.0 * 1024.0,
        "g" => 1024.0 * 1024.0 * 1024.0,
        "t" => 1024f64.powi(4),
        _ => return Err(format!("bad size unit '{unit}'")),
    };
    Ok(Some((v * mul) as u64))
}

/// Parse `YYYY-MM-DD` (local midnight) or a relative age like `30d`, `6m`, `2y`, `3w`
/// (meaning "that long ago").
pub fn parse_date(s: &str) -> Result<Option<i64>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let dt = d.and_hms_opt(0, 0, 0).unwrap();
        return Ok(Some(Local.from_local_datetime(&dt).earliest().map(|x| x.timestamp()).unwrap_or(0)));
    }
    let lower = s.to_ascii_lowercase();
    let (num, unit) = lower.split_at(lower.len().saturating_sub(1));
    if let Ok(n) = num.parse::<i64>() {
        let days = match unit {
            "d" => n,
            "w" => n * 7,
            "m" => n * 30,
            "y" => n * 365,
            "h" => return Ok(Some(Local::now().timestamp() - n * 3600)),
            _ => return Err(format!("bad date '{s}'")),
        };
        return Ok(Some(Local::now().timestamp() - days * 86400));
    }
    Err(format!("bad date '{s}' (use YYYY-MM-DD or 30d / 6m / 1y)"))
}

fn build_globs(patterns: &str, case_sensitive: bool) -> Result<Option<GlobSet>, String> {
    let mut b = GlobSetBuilder::new();
    let mut any = false;
    for p in patterns.split([';', ',', '|']).map(str::trim).filter(|p| !p.is_empty()) {
        // A bare word means "contains".
        let pat = if p.contains(['*', '?', '[']) { p.to_string() } else { format!("*{p}*") };
        let g = GlobBuilder::new(&pat)
            .case_insensitive(!case_sensitive)
            .literal_separator(false)
            .backslash_escape(false)
            .build()
            .map_err(|e| e.to_string())?;
        b.add(g);
        any = true;
    }
    if !any {
        return Ok(None);
    }
    b.build().map(Some).map_err(|e| e.to_string())
}

impl FilterSpec {
    pub fn compile(&self, use_alloc: bool) -> Result<Compiled, String> {
        let name = if self.name.trim().is_empty() {
            None
        } else if self.use_regex {
            Some(NameMatch::Regex(
                RegexBuilder::new(self.name.trim())
                    .case_insensitive(!self.case_sensitive)
                    .build()
                    .map_err(|e| e.to_string())?,
            ))
        } else {
            build_globs(&self.name, self.case_sensitive)?.map(NameMatch::Glob)
        };
        let exts = split_exts(&self.exts);
        Ok(Compiled {
            name,
            exclude: build_globs(&self.exclude, self.case_sensitive)?,
            exts,
            min_size: parse_size(&self.min_size)?,
            max_size: parse_size(&self.max_size)?,
            created: (parse_date(&self.created_after)?, parse_date(&self.created_before)?),
            modified: (parse_date(&self.modified_after)?, parse_date(&self.modified_before)?),
            accessed: (parse_date(&self.accessed_after)?, parse_date(&self.accessed_before)?),
            kind: self.kind,
            include_hidden: self.include_hidden,
            include_system: self.include_system,
            case_sensitive: self.case_sensitive,
            use_alloc,
            ..Default::default()
        })
    }
}

fn split_exts(s: &str) -> Vec<String> {
    s.split([';', ',', ' ', '|'])
        .map(|e| e.trim().trim_start_matches("*.").trim_start_matches('.').to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect()
}

/// Parse the search box query.
pub fn parse_query(q: &str, use_alloc: bool) -> Result<Compiled, String> {
    let mut c = Compiled { kind: Kind::Both, include_hidden: true, include_system: true, use_alloc, ..Default::default() };
    let mut globs = Vec::new();
    for tok in q.split_whitespace() {
        let (key, val) = match tok.split_once(':') {
            Some((k, v)) if !k.is_empty() && k.chars().all(|ch| ch.is_ascii_alphabetic()) && k.len() > 1 => {
                (k.to_ascii_lowercase(), v)
            }
            _ => (String::new(), tok),
        };
        let cmp = |v: &str| -> (char, String) {
            let v = v.trim();
            if let Some(r) = v.strip_prefix(">=") {
                ('>', r.into())
            } else if let Some(r) = v.strip_prefix("<=") {
                ('<', r.into())
            } else if let Some(r) = v.strip_prefix('>') {
                ('>', r.into())
            } else if let Some(r) = v.strip_prefix('<') {
                ('<', r.into())
            } else {
                ('=', v.into())
            }
        };
        match key.as_str() {
            "" => {
                if val.contains(['*', '?']) {
                    globs.push(val.to_string());
                } else {
                    c.substrings.push(val.to_lowercase());
                }
            }
            "ext" => c.exts.extend(split_exts(val)),
            "size" => {
                let (op, v) = cmp(val);
                let n = parse_size(&v)?;
                match op {
                    '>' => c.min_size = n,
                    '<' => c.max_size = n,
                    _ => {
                        c.min_size = n;
                        c.max_size = n;
                    }
                }
            }
            "modified" | "dm" | "created" | "dc" | "accessed" | "da" => {
                let (op, v) = cmp(val);
                let t = parse_date(&v)?;
                // a relative age ("30d") reads naturally inverted: modified:<30d == within last 30 days
                let relative = !v.contains('-');
                let slot = match key.as_str() {
                    "modified" | "dm" => &mut c.modified,
                    "created" | "dc" => &mut c.created,
                    _ => &mut c.accessed,
                };
                match (op, relative) {
                    ('>', false) | ('<', true) => slot.0 = t,
                    ('<', false) | ('>', true) => slot.1 = t,
                    _ => {
                        slot.0 = t;
                        slot.1 = t.map(|x| x + 86400);
                    }
                }
            }
            "older" => c.modified.1 = parse_date(val)?,
            "newer" => c.modified.0 = parse_date(val)?,
            "file" | "files" => c.kind = Kind::Files,
            "folder" | "folders" | "dir" => c.kind = Kind::Folders,
            "path" => c.path_substrings.push(val.to_lowercase()),
            "regex" => {
                c.name = Some(NameMatch::Regex(
                    RegexBuilder::new(val).case_insensitive(true).build().map_err(|e| e.to_string())?,
                ))
            }
            _ => c.substrings.push(tok.to_lowercase()),
        }
    }
    if !globs.is_empty() && c.name.is_none() {
        c.name = build_globs(&globs.join(";"), false)?.map(NameMatch::Glob);
    }
    Ok(c)
}

impl Compiled {
    pub fn matches(&self, t: &Tree, i: u32) -> bool {
        let n = t.node(i);
        let is_dir = n.is_dir();
        match self.kind {
            Kind::Files if is_dir => return false,
            Kind::Folders if !is_dir => return false,
            _ => {}
        }
        if !self.include_hidden && n.flags & flags::HIDDEN != 0 {
            return false;
        }
        if !self.include_system && n.flags & (flags::SYSTEM | flags::METAFILE) != 0 {
            return false;
        }
        let sz = if self.use_alloc { n.alloc } else { n.size };
        if self.min_size.is_some_and(|m| sz < m) || self.max_size.is_some_and(|m| sz > m) {
            return false;
        }
        let in_range = |v: i64, r: (Option<i64>, Option<i64>)| r.0.is_none_or(|a| v >= a) && r.1.is_none_or(|b| v < b);
        if !in_range(n.created, self.created) || !in_range(n.modified, self.modified) || !in_range(n.accessed, self.accessed) {
            return false;
        }
        if !self.exts.is_empty() {
            if is_dir {
                return false;
            }
            let e = t.ext_name(i);
            if !self.exts.iter().any(|x| x == e) {
                return false;
            }
        }
        let name = t.name(i);
        if !self.substrings.is_empty() {
            let lname = name.to_lowercase();
            if !self.substrings.iter().all(|s| lname.contains(s.as_str())) {
                return false;
            }
        }
        if let Some(ex) = &self.exclude
            && ex.is_match(name)
        {
            return false;
        }
        match &self.name {
            None => {}
            Some(NameMatch::Glob(g)) => {
                if !g.is_match(name) {
                    return false;
                }
            }
            Some(NameMatch::Regex(r)) => {
                if !r.is_match(name) {
                    return false;
                }
            }
        }
        if !self.path_substrings.is_empty() {
            let p = t.path(i).to_lowercase();
            if !self.path_substrings.iter().all(|s| p.contains(s.as_str())) {
                return false;
            }
        }
        let _ = self.case_sensitive;
        true
    }
}

/// Everything under `scope` (excluding `scope` itself) that matches, largest first.
pub fn run(t: &Tree, scope: u32, c: &Compiled, limit: usize) -> Vec<u32> {
    let mut all = Vec::new();
    t.for_each_descendant(scope, |i| {
        if i != scope {
            all.push(i);
        }
    });
    finish(t, all.into_par_iter().filter(|&i| c.matches(t, i)).collect(), c.use_alloc, limit)
}

/// Global search over the whole index (fast linear scan, parallel).
pub fn search_all(t: &Tree, c: &Compiled, limit: usize) -> Vec<u32> {
    let hits: Vec<u32> = (0..t.nodes.len() as u32)
        .into_par_iter()
        .filter(|&i| {
            let n = t.node(i);
            i != t.root && n.parent != NONE && !n.is_deleted() && c.matches(t, i)
        })
        .collect();
    finish(t, hits, c.use_alloc, limit)
}

fn finish(t: &Tree, mut v: Vec<u32>, use_alloc: bool, limit: usize) -> Vec<u32> {
    let key = |i: &u32| {
        let n = t.node(*i);
        if use_alloc { n.alloc } else { n.size }
    };
    v.par_sort_unstable_by_key(|i| std::cmp::Reverse(key(i)));
    v.truncate(limit);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("10k").unwrap(), Some(10240));
        assert_eq!(parse_size("1.5 GB").unwrap(), Some(1610612736));
        assert_eq!(parse_size("").unwrap(), None);
        assert!(parse_size("12q").is_err());
    }

    #[test]
    fn dates() {
        assert!(parse_date("2024-01-31").unwrap().is_some());
        let a = parse_date("30d").unwrap().unwrap();
        assert!(Local::now().timestamp() - a >= 30 * 86400 - 2);
        assert!(parse_date("yesterday").is_err());
    }

    #[test]
    fn query_parse() {
        let c = parse_query("ext:mp4;mkv size:>1gb holiday older:1y", false).unwrap();
        assert_eq!(c.exts, vec!["mp4", "mkv"]);
        assert_eq!(c.min_size, Some(1 << 30));
        assert_eq!(c.substrings, vec!["holiday"]);
        assert!(c.modified.1.is_some());
    }
}
