//! Paths, metadata (stars, shares, trash) and filesystem helpers.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------- errors ----------
pub struct AppError(pub StatusCode, pub String);
pub type R<T> = Result<T, AppError>;

pub fn fail(code: StatusCode, msg: impl Into<String>) -> AppError {
    AppError(code, msg.into())
}
pub fn bad(msg: impl Into<String>) -> AppError {
    fail(StatusCode::BAD_REQUEST, msg)
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind::*;
        match e.kind() {
            NotFound => fail(StatusCode::NOT_FOUND, "Not found"),
            AlreadyExists => fail(StatusCode::CONFLICT, "Something with that name already exists"),
            PermissionDenied => fail(StatusCode::FORBIDDEN, "Permission denied"),
            _ => fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    }
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

pub fn random_id(n: usize) -> String {
    use base64::Engine;
    let mut b = vec![0u8; n];
    getrandom::getrandom(&mut b).expect("OS randomness unavailable");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

// ---------- layout on disk ----------
pub struct Dirs {
    pub files: PathBuf,
    pub trash: PathBuf,
    pub tmp: PathBuf,
    pub thumbs: PathBuf,
    pub state: PathBuf,
}

impl Dirs {
    pub fn new(data: &Path) -> Self {
        Dirs {
            files: data.join("files"),
            trash: data.join("trash"),
            tmp: data.join("tmp"),
            thumbs: data.join("thumbs"),
            state: data.join("state"),
        }
    }
    pub fn create(&self) -> std::io::Result<()> {
        for d in [&self.files, &self.trash, &self.tmp, &self.thumbs, &self.state] {
            std::fs::create_dir_all(d)?;
        }
        // leftovers from interrupted uploads
        if let Ok(rd) = std::fs::read_dir(&self.tmp) {
            for e in rd.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
        Ok(())
    }
}

/// Split a user path ("/a/b") into safe components.
pub fn clean(rel: &str) -> R<Vec<String>> {
    let mut out = Vec::new();
    for c in rel.split('/') {
        if c.is_empty() || c == "." {
            continue;
        }
        if c == ".." || c.contains('\0') || c.len() > 255 {
            return Err(bad("Invalid path"));
        }
        out.push(c.to_string());
    }
    Ok(out)
}

pub fn valid_name(name: &str) -> R<String> {
    let n = name.trim();
    if n.is_empty() || n == "." || n == ".." || n.contains('/') || n.contains('\0') || n.len() > 255 {
        return Err(bad("That name isn't allowed"));
    }
    Ok(n.to_string())
}

pub fn join(base: &Path, parts: &[String]) -> PathBuf {
    let mut p = base.to_path_buf();
    for c in parts {
        p.push(c);
    }
    p
}

pub fn display(parts: &[String]) -> String {
    format!("/{}", parts.join("/"))
}

pub fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => path[..i].to_string(),
    }
}

pub fn name_of(path: &str) -> String {
    path.rsplit('/').next().unwrap_or("").to_string()
}

/// "name.ext" -> "name (1).ext" until the path is free.
pub fn unique(p: PathBuf) -> PathBuf {
    if p.symlink_metadata().is_err() {
        return p;
    }
    let parent = p.parent().map(Path::to_path_buf).unwrap_or_default();
    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let (stem, ext) = split_ext(&name);
    for n in 1.. {
        let cand = parent.join(format!("{stem} ({n}){ext}"));
        if cand.symlink_metadata().is_err() {
            return cand;
        }
    }
    unreachable!()
}

pub fn split_ext(name: &str) -> (String, String) {
    match name.rfind('.') {
        Some(i) if i > 0 => (name[..i].to_string(), name[i..].to_string()),
        _ => (name.to_string(), String::new()),
    }
}

/// Bytes of regular files under a path (symlinks are not followed).
pub fn tree_size(p: &Path) -> u64 {
    let Ok(md) = p.symlink_metadata() else { return 0 };
    if md.is_file() {
        return md.len();
    }
    if !md.is_dir() {
        return 0;
    }
    std::fs::read_dir(p)
        .map(|rd| rd.flatten().map(|e| tree_size(&e.path())).sum())
        .unwrap_or(0)
}

pub fn count_items(p: &Path) -> u64 {
    std::fs::read_dir(p).map(|rd| rd.count() as u64).unwrap_or(0)
}

pub fn mtime(md: &std::fs::Metadata) -> u64 {
    md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs())
}

/// Recursively copy a file or folder.
pub fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    let md = src.symlink_metadata()?;
    if md.is_dir() {
        std::fs::create_dir(dst)?;
        for e in std::fs::read_dir(src)?.flatten() {
            copy_tree(&e.path(), &dst.join(e.file_name()))?;
        }
    } else if md.is_file() {
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

/// Walk the drive collecting (relative path, metadata) for every entry.
pub fn walk(root: &Path, limit: usize, mut f: impl FnMut(&str, &std::fs::Metadata) -> bool) {
    let mut stack = vec![(root.to_path_buf(), String::new())];
    let mut seen = 0usize;
    while let Some((dir, rel)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > limit {
                return;
            }
            let name = e.file_name().to_string_lossy().to_string();
            let path = format!("{rel}/{name}");
            let Ok(md) = e.path().symlink_metadata() else { continue };
            if md.is_symlink() {
                continue;
            }
            if !f(&path, &md) {
                return;
            }
            if md.is_dir() {
                stack.push((e.path(), path));
            }
        }
    }
}

pub fn kind_of(name: &str, dir: bool) -> &'static str {
    if dir {
        return "folder";
    }
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "svg" | "heic" | "heif" | "tif" | "tiff" | "avif" => "image",
        "mp4" | "mov" | "mkv" | "webm" | "avi" | "m4v" | "wmv" => "video",
        "mp3" | "wav" | "m4a" | "flac" | "ogg" | "aac" | "opus" => "audio",
        "pdf" => "pdf",
        "doc" | "docx" | "odt" | "rtf" | "txt" | "md" | "pages" => "document",
        "xls" | "xlsx" | "ods" | "csv" | "tsv" | "numbers" => "spreadsheet",
        "ppt" | "pptx" | "odp" | "key" => "presentation",
        "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "bz2" | "xz" => "archive",
        "rs" | "py" | "js" | "ts" | "java" | "c" | "cpp" | "h" | "go" | "sql" | "sh" | "json" | "yaml" | "yml" | "toml" | "html" | "css" | "xml" => "code",
        _ => "file",
    }
}

// ---------- metadata ----------
#[derive(Serialize, Deserialize, Clone)]
pub struct Share {
    pub path: String,
    pub dir: bool,
    pub created: u64,
    pub expires: Option<u64>,
    #[serde(default)]
    pub downloads: u64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct TrashItem {
    pub name: String,
    pub from: String,
    pub deleted: u64,
    pub dir: bool,
    pub size: u64,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Meta {
    #[serde(default)]
    pub stars: BTreeSet<String>,
    #[serde(default)]
    pub shares: BTreeMap<String, Share>,
    #[serde(default)]
    pub trash: BTreeMap<String, TrashItem>,
}

impl Meta {
    pub fn load(path: &Path) -> Meta {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap_or_default())?;
        std::fs::rename(tmp, path)
    }

    /// Keep stars and share links pointing at the right place after a move/rename.
    pub fn moved(&mut self, from: &str, to: &str) {
        let remap = |p: &str| -> Option<String> {
            if p == from {
                Some(to.to_string())
            } else {
                p.strip_prefix(&format!("{from}/")).map(|rest| format!("{to}/{rest}"))
            }
        };
        let stars: Vec<String> = self.stars.iter().cloned().collect();
        for s in stars {
            if let Some(n) = remap(&s) {
                self.stars.remove(&s);
                self.stars.insert(n);
            }
        }
        for sh in self.shares.values_mut() {
            if let Some(n) = remap(&sh.path) {
                sh.path = n;
            }
        }
    }

    /// Forget stars and links under a path that left the drive.
    pub fn removed(&mut self, path: &str) {
        let under = |p: &str| p == path || p.starts_with(&format!("{path}/"));
        self.stars.retain(|s| !under(s));
        self.shares.retain(|_, sh| !under(&sh.path));
    }

    pub fn share_for(&self, path: &str) -> Option<(String, Share)> {
        self.shares
            .iter()
            .find(|(_, s)| s.path == path && s.expires.is_none_or(|e| e > now()))
            .map(|(k, v)| (k.clone(), v.clone()))
    }
}

/// UTC civil date from unix seconds (for ZIP timestamps).
pub fn civil(secs: u64) -> (u16, u8, u8, u8, u8, u8) {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as u16, m as u8, d as u8, (rem / 3600) as u8, ((rem % 3600) / 60) as u8, (rem % 60) as u8)
}
