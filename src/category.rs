//! Friendly file categories ("Videos", "Images", ...) derived from extensions.

use crate::model::{Tree, flags};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Category {
    Videos,
    Images,
    Music,
    Documents,
    Archives,
    Apps,
    Code,
    Games,
    System,
    Other,
}

pub const ALL: [Category; 10] = [
    Category::Videos,
    Category::Images,
    Category::Music,
    Category::Documents,
    Category::Archives,
    Category::Apps,
    Category::Code,
    Category::Games,
    Category::System,
    Category::Other,
];

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Videos => "Videos",
            Category::Images => "Images",
            Category::Music => "Music",
            Category::Documents => "Documents",
            Category::Archives => "Archives & disk images",
            Category::Apps => "Apps & libraries",
            Category::Code => "Code & dev",
            Category::Games => "Game data",
            Category::System => "System",
            Category::Other => "Other",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Category::Videos => "🎬",
            Category::Images => "🖼",
            Category::Music => "🎵",
            Category::Documents => "📄",
            Category::Archives => "📦",
            Category::Apps => "⚙",
            Category::Code => "⌨",
            Category::Games => "🎮",
            Category::System => "🖥",
            Category::Other => "…",
        }
    }

    pub fn index(self) -> usize {
        ALL.iter().position(|&c| c == self).unwrap()
    }

    /// Query snippet for the search box that selects this category.
    pub fn exts(self) -> &'static [&'static str] {
        match self {
            Category::Videos => &["mp4", "mkv", "avi", "mov", "wmv", "m4v", "webm", "flv", "mpg", "mpeg", "ts", "m2ts", "vob", "3gp"],
            Category::Images => &["jpg", "jpeg", "png", "gif", "bmp", "tif", "tiff", "webp", "heic", "heif", "raw", "cr2", "cr3", "nef", "arw", "dng", "psd", "svg", "ico", "avif"],
            Category::Music => &["mp3", "flac", "wav", "aac", "m4a", "ogg", "wma", "opus", "aiff", "mid"],
            Category::Documents => &["pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "txt", "rtf", "md", "csv", "epub", "pst", "ost", "one", "msg", "eml"],
            Category::Archives => &["zip", "7z", "rar", "tar", "gz", "bz2", "xz", "zst", "iso", "img", "vhd", "vhdx", "vmdk", "vdi", "qcow2", "wim", "esd", "cab", "dmg"],
            Category::Apps => &["exe", "dll", "msi", "msix", "appx", "sys", "ocx", "so", "jar", "apk", "pak", "node", "lib", "a", "pdb"],
            Category::Code => &["rs", "c", "h", "cpp", "hpp", "cs", "java", "js", "mjs", "ts", "tsx", "jsx", "py", "go", "rb", "php", "json", "xml", "yml", "yaml", "toml", "html", "css", "scss", "sql", "o", "obj", "rlib", "rmeta", "class", "pyc", "map", "lock", "sh", "ps1"],
            Category::Games => &["bsa", "ba2", "vpk", "forge", "ucas", "utoc", "bundle", "assets", "resource", "upk", "uasset", "umap", "big", "wad"],
            Category::System => &["etl", "evtx", "log", "dmp", "tmp", "bak", "cat", "mum", "manifest", "msp", "db", "dat", "edb", "sdi", "ldf", "mdf"],
            Category::Other => &[],
        }
    }
}

pub fn of_ext(ext: &str) -> Category {
    for c in ALL {
        if c.exts().contains(&ext) {
            return c;
        }
    }
    Category::Other
}

/// Category for every extension index of a tree.
pub fn ext_categories(t: &Tree) -> Vec<Category> {
    t.exts.iter().map(|e| of_ext(&e.name)).collect()
}

/// Category of a single file node (system/metadata files override the extension).
pub fn of_node(t: &Tree, cats: &[Category], i: u32) -> Category {
    let n = t.node(i);
    if n.flags & flags::METAFILE != 0 {
        return Category::System;
    }
    let c = cats.get(n.ext as usize).copied().unwrap_or(Category::Other);
    if c == Category::Other && n.flags & flags::SYSTEM != 0 {
        return Category::System;
    }
    c
}

/// (size, alloc, count) per category below `root`.
pub fn breakdown(t: &Tree, cats: &[Category], root: u32) -> [(u64, u64, u64); 10] {
    let mut out = [(0u64, 0u64, 0u64); 10];
    t.for_each_descendant(root, |i| {
        let n = t.node(i);
        if !n.is_dir() {
            let k = of_node(t, cats, i).index();
            out[k].0 += n.size;
            out[k].1 += n.alloc;
            out[k].2 += 1;
        }
    });
    out
}
