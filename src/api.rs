//! JSON API used by the web app and the public share page.

use crate::media::{make_thumb, serve, thumbable, zip_response};
use crate::store::*;
use crate::{Authed, Shared};
use axum::body::Body;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use tokio::io::{AsyncWriteExt, BufWriter};

#[derive(Deserialize)]
pub struct PathQ {
    #[serde(default)]
    path: String,
}

#[derive(Deserialize)]
pub struct PathsBody {
    paths: Vec<String>,
}

fn entry(name: &str, path: &str, md: &std::fs::Metadata, stars: &HashSet<String>, shared: &HashSet<String>) -> Value {
    let dir = md.is_dir();
    json!({
        "name": name,
        "path": path,
        "dir": dir,
        "size": if dir { 0 } else { md.len() },
        "mtime": mtime(md),
        "kind": kind_of(name, dir),
        "starred": stars.contains(path),
        "shared": shared.contains(path),
        "thumb": !dir && thumbable(name),
    })
}

async fn marks(app: &Shared) -> (HashSet<String>, HashSet<String>) {
    let m = app.meta.lock().await;
    let t = now();
    (
        m.stars.iter().cloned().collect(),
        m.shares.values().filter(|s| s.expires.is_none_or(|e| e > t)).map(|s| s.path.clone()).collect(),
    )
}

fn sort_entries(v: &mut [Value]) {
    v.sort_by(|a, b| {
        b["dir"].as_bool().cmp(&a["dir"].as_bool()).then_with(|| {
            a["name"].as_str().unwrap_or("").to_lowercase().cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
        })
    });
}

fn usage(app: &Shared) -> Value {
    json!({ "used": app.used.load(Ordering::Relaxed), "quota": app.quota })
}

// ---------- browsing ----------
pub async fn list(_a: Authed, State(app): State<Shared>, Query(q): Query<PathQ>) -> R<Json<Value>> {
    let parts = clean(&q.path)?;
    let base = display(&parts);
    let dir = join(&app.dirs.files, &parts);
    let (stars, shared) = marks(&app).await;
    let mut rd = tokio::fs::read_dir(&dir).await?;
    let mut out = Vec::new();
    while let Some(e) = rd.next_entry().await? {
        let name = e.file_name().to_string_lossy().to_string();
        let Ok(md) = e.metadata().await else { continue };
        let path = if base == "/" { format!("/{name}") } else { format!("{base}/{name}") };
        out.push(entry(&name, &path, &md, &stars, &shared));
    }
    sort_entries(&mut out);
    Ok(Json(json!({ "path": base, "entries": out, "usage": usage(&app) })))
}

pub async fn recent(_a: Authed, State(app): State<Shared>) -> R<Json<Value>> {
    let root = app.dirs.files.clone();
    let (stars, shared) = marks(&app).await;
    let out = tokio::task::spawn_blocking(move || {
        let mut files: Vec<(u64, String, std::fs::Metadata)> = Vec::new();
        walk(&root, 300_000, |p, md| {
            if md.is_file() {
                files.push((mtime(md), p.to_string(), md.clone()));
            }
            true
        });
        files.sort_by(|a, b| b.0.cmp(&a.0));
        files.truncate(100);
        files.into_iter().map(|(_, p, md)| entry(&name_of(&p), &p, &md, &stars, &shared)).collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    Ok(Json(json!({ "entries": out, "usage": usage(&app) })))
}

#[derive(Deserialize)]
pub struct SearchQ {
    q: String,
}

pub async fn search(_a: Authed, State(app): State<Shared>, Query(q): Query<SearchQ>) -> R<Json<Value>> {
    let needle = q.q.trim().to_lowercase();
    if needle.is_empty() {
        return Ok(Json(json!({ "entries": [], "usage": usage(&app) })));
    }
    let root = app.dirs.files.clone();
    let (stars, shared) = marks(&app).await;
    let mut out = tokio::task::spawn_blocking(move || {
        let mut v = Vec::new();
        walk(&root, 300_000, |p, md| {
            let name = name_of(p);
            if name.to_lowercase().contains(&needle) {
                v.push(entry(&name, p, md, &stars, &shared));
            }
            v.len() < 500
        });
        v
    })
    .await
    .unwrap_or_default();
    sort_entries(&mut out);
    Ok(Json(json!({ "entries": out, "usage": usage(&app) })))
}

pub async fn starred(_a: Authed, State(app): State<Shared>) -> R<Json<Value>> {
    let (stars, shared) = marks(&app).await;
    let mut out = Vec::new();
    let mut gone = Vec::new();
    for p in &stars {
        let parts = clean(p)?;
        match join(&app.dirs.files, &parts).metadata() {
            Ok(md) => out.push(entry(&name_of(p), p, &md, &stars, &shared)),
            Err(_) => gone.push(p.clone()),
        }
    }
    if !gone.is_empty() {
        let mut m = app.meta.lock().await;
        for g in gone {
            m.stars.remove(&g);
        }
        app.save(&m);
    }
    sort_entries(&mut out);
    Ok(Json(json!({ "entries": out, "usage": usage(&app) })))
}

pub async fn info(_a: Authed, State(app): State<Shared>, Query(q): Query<PathQ>) -> R<Json<Value>> {
    let parts = clean(&q.path)?;
    let p = join(&app.dirs.files, &parts);
    let md = tokio::fs::metadata(&p).await?;
    let (stars, shared) = marks(&app).await;
    let path = display(&parts);
    let mut v = entry(&name_of(&path), &path, &md, &stars, &shared);
    if md.is_dir() {
        let (items, size) = tokio::task::spawn_blocking(move || (count_items(&p), tree_size(&p))).await.unwrap_or((0, 0));
        v["items"] = json!(items);
        v["size"] = json!(size);
    }
    let created = md.created().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs());
    v["created"] = json!(created);
    let m = app.meta.lock().await;
    v["share"] = json!(m.share_for(&path).map(|(id, s)| json!({ "id": id, "url": format!("/s/{id}"), "expires": s.expires })));
    Ok(Json(v))
}

// ---------- changes ----------
pub async fn mkdir(_a: Authed, State(app): State<Shared>, Json(b): Json<PathQ>) -> R<Json<Value>> {
    let parts = clean(&b.path)?;
    if parts.is_empty() {
        return Err(bad("Invalid folder"));
    }
    valid_name(parts.last().unwrap())?;
    let p = join(&app.dirs.files, &parts);
    if p.symlink_metadata().is_ok() {
        return Err(fail(StatusCode::CONFLICT, "A file or folder with that name already exists"));
    }
    tokio::fs::create_dir_all(&p).await?;
    Ok(Json(json!({ "path": display(&parts) })))
}

#[derive(Deserialize)]
pub struct RenameBody {
    path: String,
    name: String,
}

pub async fn rename(_a: Authed, State(app): State<Shared>, Json(b): Json<RenameBody>) -> R<Json<Value>> {
    let parts = clean(&b.path)?;
    if parts.is_empty() {
        return Err(bad("Can't rename that"));
    }
    let name = valid_name(&b.name)?;
    let src = join(&app.dirs.files, &parts);
    src.symlink_metadata()?;
    let mut to_parts = parts.clone();
    *to_parts.last_mut().unwrap() = name;
    let dst = join(&app.dirs.files, &to_parts);
    let case_only = parts.last().unwrap().to_lowercase() == to_parts.last().unwrap().to_lowercase();
    if dst.symlink_metadata().is_ok() && !case_only {
        return Err(fail(StatusCode::CONFLICT, "Something with that name already exists here"));
    }
    tokio::fs::rename(&src, &dst).await?;
    let mut m = app.meta.lock().await;
    m.moved(&display(&parts), &display(&to_parts));
    app.save(&m);
    Ok(Json(json!({ "path": display(&to_parts) })))
}

#[derive(Deserialize)]
pub struct MoveBody {
    paths: Vec<String>,
    dest: String,
}

pub async fn mv(_a: Authed, State(app): State<Shared>, Json(b): Json<MoveBody>) -> R<Json<Value>> {
    let dest = clean(&b.dest)?;
    let dest_abs = join(&app.dirs.files, &dest);
    if !dest_abs.is_dir() {
        return Err(fail(StatusCode::NOT_FOUND, "That folder doesn't exist"));
    }
    let dest_s = display(&dest);
    let mut moved = 0;
    for p in &b.paths {
        let parts = clean(p)?;
        if parts.is_empty() {
            continue;
        }
        let from = display(&parts);
        if dest_s == from || dest_s.starts_with(&format!("{from}/")) {
            return Err(bad("You can't move a folder into itself"));
        }
        if parent_of(&from) == dest_s {
            continue;
        }
        let src = join(&app.dirs.files, &parts);
        let dst = unique(dest_abs.join(parts.last().unwrap()));
        tokio::fs::rename(&src, &dst).await?;
        let to = format!("{}/{}", if dest_s == "/" { "" } else { &dest_s }, dst.file_name().unwrap().to_string_lossy());
        let mut m = app.meta.lock().await;
        m.moved(&from, &to);
        app.save(&m);
        moved += 1;
    }
    Ok(Json(json!({ "moved": moved })))
}

#[derive(Deserialize)]
pub struct CopyBody {
    paths: Vec<String>,
    dest: Option<String>,
}

pub async fn copy(_a: Authed, State(app): State<Shared>, Json(b): Json<CopyBody>) -> R<Json<Value>> {
    let mut jobs = Vec::new();
    let mut total = 0u64;
    for p in &b.paths {
        let parts = clean(p)?;
        if parts.is_empty() {
            continue;
        }
        let src = join(&app.dirs.files, &parts);
        src.symlink_metadata()?;
        let from = display(&parts);
        let dest_s = match &b.dest {
            Some(d) => display(&clean(d)?),
            None => parent_of(&from),
        };
        if dest_s == from || dest_s.starts_with(&format!("{from}/")) {
            return Err(bad("You can't copy a folder into itself"));
        }
        let dest_abs = join(&app.dirs.files, &clean(&dest_s)?);
        if !dest_abs.is_dir() {
            return Err(fail(StatusCode::NOT_FOUND, "That folder doesn't exist"));
        }
        let name = parts.last().unwrap();
        let new_name = if parent_of(&from) == dest_s { format!("Copy of {name}") } else { name.clone() };
        let s = src.clone();
        total += tokio::task::spawn_blocking(move || tree_size(&s)).await.unwrap_or(0);
        jobs.push((src, dest_abs.join(new_name)));
    }
    if total > app.free() {
        return Err(fail(StatusCode::INSUFFICIENT_STORAGE, "Not enough space left in your drive"));
    }
    let n = jobs.len();
    tokio::task::spawn_blocking(move || {
        for (src, dst) in jobs {
            copy_tree(&src, &unique(dst))?;
        }
        Ok::<_, std::io::Error>(())
    })
    .await
    .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    app.used.fetch_add(total, Ordering::Relaxed);
    Ok(Json(json!({ "copied": n })))
}

#[derive(Deserialize)]
pub struct StarBody {
    paths: Vec<String>,
    on: bool,
}

pub async fn star(_a: Authed, State(app): State<Shared>, Json(b): Json<StarBody>) -> R<Json<Value>> {
    let mut m = app.meta.lock().await;
    for p in &b.paths {
        let path = display(&clean(p)?);
        if b.on {
            m.stars.insert(path);
        } else {
            m.stars.remove(&path);
        }
    }
    app.save(&m);
    Ok(Json(json!({ "ok": true })))
}

/// Move one drive path into Trash. Returns the trash id.
pub async fn trash_one(app: &Shared, parts: &[String]) -> R<String> {
    if parts.is_empty() {
        return Err(bad("Can't delete that"));
    }
    let path = display(parts);
    let src = join(&app.dirs.files, parts);
    let md = src.symlink_metadata()?;
    let id = random_id(12);
    let dst = app.dirs.trash.join(&id);
    tokio::fs::rename(&src, &dst).await?;
    let size = tokio::task::spawn_blocking(move || tree_size(&dst)).await.unwrap_or(0);
    let mut m = app.meta.lock().await;
    m.trash.insert(
        id.clone(),
        TrashItem { name: name_of(&path), from: parent_of(&path), deleted: now(), dir: md.is_dir(), size },
    );
    m.removed(&path);
    app.save(&m);
    Ok(id)
}

pub async fn delete(_a: Authed, State(app): State<Shared>, Json(b): Json<PathsBody>) -> R<Json<Value>> {
    let mut ids = Vec::new();
    for p in &b.paths {
        ids.push(trash_one(&app, &clean(p)?).await?);
    }
    Ok(Json(json!({ "ids": ids })))
}

pub async fn trash_list(_a: Authed, State(app): State<Shared>) -> R<Json<Value>> {
    let m = app.meta.lock().await;
    let mut v: Vec<Value> = m
        .trash
        .iter()
        .filter(|(id, _)| app.dirs.trash.join(id).symlink_metadata().is_ok())
        .map(|(id, t)| {
            json!({ "id": id, "name": t.name, "from": t.from, "deleted": t.deleted, "dir": t.dir, "size": t.size,
                    "kind": kind_of(&t.name, t.dir), "mtime": t.deleted, "path": format!("{}/{}", if t.from == "/" { "" } else { &t.from }, t.name) })
        })
        .collect();
    v.sort_by(|a, b| b["deleted"].as_u64().cmp(&a["deleted"].as_u64()));
    Ok(Json(json!({ "entries": v, "usage": usage(&app) })))
}

#[derive(Deserialize)]
pub struct IdsBody {
    ids: Vec<String>,
}

pub async fn trash_restore(_a: Authed, State(app): State<Shared>, Json(b): Json<IdsBody>) -> R<Json<Value>> {
    let mut m = app.meta.lock().await;
    let mut restored = Vec::new();
    for id in &b.ids {
        let Some(t) = m.trash.get(id).cloned() else { continue };
        let src = app.dirs.trash.join(id);
        let dir = join(&app.dirs.files, &clean(&t.from)?);
        tokio::fs::create_dir_all(&dir).await?;
        let dst = unique(dir.join(&t.name));
        tokio::fs::rename(&src, &dst).await?;
        m.trash.remove(id);
        restored.push(format!("{}/{}", if t.from == "/" { "" } else { &t.from }, dst.file_name().unwrap().to_string_lossy()));
    }
    app.save(&m);
    Ok(Json(json!({ "restored": restored })))
}

pub async fn purge(app: &Shared, ids: &[String]) -> R<usize> {
    let mut m = app.meta.lock().await;
    let mut n = 0;
    for id in ids {
        let p = app.dirs.trash.join(id);
        if let Ok(md) = p.symlink_metadata() {
            let size = { let p = p.clone(); tokio::task::spawn_blocking(move || tree_size(&p)).await.unwrap_or(0) };
            if md.is_dir() {
                tokio::fs::remove_dir_all(&p).await?;
            } else {
                tokio::fs::remove_file(&p).await?;
            }
            let cur = app.used.load(Ordering::Relaxed);
            app.used.store(cur.saturating_sub(size), Ordering::Relaxed);
        }
        m.trash.remove(id);
        n += 1;
    }
    app.save(&m);
    Ok(n)
}

pub async fn trash_delete(_a: Authed, State(app): State<Shared>, Json(b): Json<IdsBody>) -> R<Json<Value>> {
    let n = purge(&app, &b.ids).await?;
    Ok(Json(json!({ "deleted": n })))
}

pub async fn trash_empty(_a: Authed, State(app): State<Shared>) -> R<Json<Value>> {
    let mut ids: Vec<String> = app.meta.lock().await.trash.keys().cloned().collect();
    // also anything on disk that lost its record
    if let Ok(rd) = std::fs::read_dir(&app.dirs.trash) {
        ids.extend(rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()));
    }
    ids.sort();
    ids.dedup();
    let n = purge(&app, &ids).await?;
    Ok(Json(json!({ "deleted": n })))
}

// ---------- upload ----------
#[derive(Deserialize)]
pub struct UploadQ {
    path: String,
    #[serde(default)]
    mode: String,
}

pub async fn upload(_a: Authed, State(app): State<Shared>, Query(q): Query<UploadQ>, headers: HeaderMap, body: Body) -> R<Json<Value>> {
    let parts = clean(&q.path)?;
    if parts.is_empty() {
        return Err(bad("Invalid upload location"));
    }
    valid_name(parts.last().unwrap())?;
    let free = app.free();
    let declared = headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > free) {
        return Err(fail(StatusCode::INSUFFICIENT_STORAGE, "Not enough space left in your drive"));
    }
    let target = join(&app.dirs.files, &parts);
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = app.dirs.tmp.join(random_id(12));
    let mut file = BufWriter::with_capacity(256 * 1024, tokio::fs::File::create(&tmp).await?);
    let mut stream = body.into_data_stream();
    let mut written: u64 = 0;
    let result: R<()> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| bad("Upload was interrupted"))?;
            written += chunk.len() as u64;
            if written > free {
                return Err(fail(StatusCode::INSUFFICIENT_STORAGE, "Not enough space left in your drive"));
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        Ok(())
    }
    .await;
    drop(file);
    if let Err(e) = result {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    let final_path = if q.mode == "replace" && target.is_file() {
        // keep the old version recoverable in Trash, like a version history
        trash_one(&app, &parts).await?;
        target
    } else {
        unique(target)
    };
    tokio::fs::rename(&tmp, &final_path).await?;
    app.used.fetch_add(written, Ordering::Relaxed);
    let name = final_path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    Ok(Json(json!({ "name": name, "size": written })))
}

// ---------- download / preview ----------
#[derive(Deserialize)]
pub struct FileQ {
    #[serde(default)]
    path: String,
    dl: Option<u8>,
}

pub async fn file(_a: Authed, State(app): State<Shared>, Query(q): Query<FileQ>, headers: HeaderMap) -> R<Response> {
    let parts = clean(&q.path)?;
    serve(join(&app.dirs.files, &parts), headers, q.dl == Some(1)).await
}

pub async fn thumb_of(app: &Shared, abs: PathBuf, key_path: &str) -> R<Response> {
    let name = abs.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if !thumbable(&name) {
        return Err(fail(StatusCode::NOT_FOUND, "No thumbnail"));
    }
    let md = tokio::fs::metadata(&abs).await?;
    if md.len() > 80 * 1024 * 1024 {
        return Err(fail(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Too large for a thumbnail"));
    }
    let key = format!("{:x}", Sha256::digest(format!("{key_path}|{}|{}", md.len(), mtime(&md))));
    let out = app.dirs.thumbs.join(format!("{}.jpg", &key[..32]));
    if !out.is_file() {
        let _permit = app.thumb_gate.acquire().await.map_err(|_| bad("busy"))?;
        if !out.is_file() {
            let o = out.clone();
            tokio::task::spawn_blocking(move || make_thumb(&abs, &o))
                .await
                .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                .map_err(|e| fail(StatusCode::UNSUPPORTED_MEDIA_TYPE, e))?;
        }
    }
    let bytes = tokio::fs::read(&out).await?;
    let mut res = Response::new(Body::from(bytes));
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=604800"));
    Ok(res)
}

pub async fn thumb(_a: Authed, State(app): State<Shared>, Query(q): Query<PathQ>) -> R<Response> {
    let parts = clean(&q.path)?;
    thumb_of(&app, join(&app.dirs.files, &parts), &display(&parts)).await
}

#[derive(Deserialize)]
pub struct ZipQ {
    paths: String,
}

pub async fn zip(_a: Authed, State(app): State<Shared>, Query(q): Query<ZipQ>) -> R<Response> {
    let paths: Vec<String> = serde_json::from_str(&q.paths).map_err(|_| bad("Invalid selection"))?;
    let mut items = Vec::new();
    for p in &paths {
        let parts = clean(p)?;
        let abs = join(&app.dirs.files, &parts);
        abs.symlink_metadata()?;
        let name = if parts.is_empty() { "My Drive".to_string() } else { parts.last().unwrap().clone() };
        items.push((abs, name));
    }
    if items.is_empty() {
        return Err(bad("Nothing selected"));
    }
    let filename = if items.len() == 1 {
        format!("{}.zip", crate::store::split_ext(&items[0].1).0)
    } else {
        format!("MyDrive-{}.zip", { let (y, m, d, ..) = civil(now()); format!("{y}-{m:02}-{d:02}") })
    };
    Ok(zip_response(items, &filename))
}

// ---------- sharing ----------
#[derive(Deserialize)]
pub struct ShareBody {
    path: String,
    days: Option<u64>,
}

pub async fn share(_a: Authed, State(app): State<Shared>, Json(b): Json<ShareBody>) -> R<Json<Value>> {
    let parts = clean(&b.path)?;
    if parts.is_empty() {
        return Err(bad("Pick a file or folder to share"));
    }
    let path = display(&parts);
    let md = join(&app.dirs.files, &parts).metadata()?;
    let expires = match b.days {
        Some(0) | None => None,
        Some(d) => Some(now() + d.min(3650) * 86400),
    };
    let mut m = app.meta.lock().await;
    let id = match m.share_for(&path) {
        Some((id, _)) => id,
        None => random_id(16),
    };
    let prev_downloads = m.shares.get(&id).map_or(0, |s| s.downloads);
    m.shares.insert(id.clone(), Share { path, dir: md.is_dir(), created: now(), expires, downloads: prev_downloads });
    app.save(&m);
    Ok(Json(json!({ "id": id, "url": format!("/s/{id}"), "expires": expires })))
}

pub async fn shares(_a: Authed, State(app): State<Shared>) -> R<Json<Value>> {
    let mut m = app.meta.lock().await;
    let t = now();
    let before = m.shares.len();
    m.shares.retain(|_, s| s.expires.is_none_or(|e| e > t));
    if m.shares.len() != before {
        app.save(&m);
    }
    let mut v: Vec<Value> = m
        .shares
        .iter()
        .map(|(id, s)| {
            json!({ "id": id, "url": format!("/s/{id}"), "path": s.path, "name": name_of(&s.path), "dir": s.dir,
                    "kind": kind_of(&name_of(&s.path), s.dir), "created": s.created, "expires": s.expires, "downloads": s.downloads,
                    "mtime": s.created })
        })
        .collect();
    v.sort_by(|a, b| b["created"].as_u64().cmp(&a["created"].as_u64()));
    Ok(Json(json!({ "entries": v, "usage": usage(&app) })))
}

#[derive(Deserialize)]
pub struct IdBody {
    id: String,
}

pub async fn unshare(_a: Authed, State(app): State<Shared>, Json(b): Json<IdBody>) -> R<Json<Value>> {
    let mut m = app.meta.lock().await;
    m.shares.remove(&b.id);
    app.save(&m);
    Ok(Json(json!({ "ok": true })))
}

// ---------- storage ----------
pub async fn storage(_a: Authed, State(app): State<Shared>) -> R<Json<Value>> {
    let root = app.dirs.files.clone();
    let trash = app.dirs.trash.clone();
    let (by_kind, largest, files, trash_bytes) = tokio::task::spawn_blocking(move || {
        let mut by_kind: BTreeMap<&'static str, u64> = BTreeMap::new();
        let mut largest: Vec<(u64, String)> = Vec::new();
        let mut files = 0u64;
        walk(&root, 1_000_000, |p, md| {
            if md.is_file() {
                files += 1;
                let k = match kind_of(&name_of(p), false) {
                    "image" => "Photos & images",
                    "video" => "Videos",
                    "audio" => "Audio",
                    "pdf" | "document" | "spreadsheet" | "presentation" => "Documents",
                    "archive" => "Archives",
                    _ => "Other",
                };
                *by_kind.entry(k).or_default() += md.len();
                largest.push((md.len(), p.to_string()));
                if largest.len() > 200 {
                    largest.sort_by(|a, b| b.0.cmp(&a.0));
                    largest.truncate(25);
                }
            }
            true
        });
        largest.sort_by(|a, b| b.0.cmp(&a.0));
        largest.truncate(25);
        (by_kind, largest, files, tree_size(&trash))
    })
    .await
    .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let largest: Vec<Value> = largest
        .into_iter()
        .map(|(s, p)| json!({ "path": p, "name": name_of(&p), "size": s, "kind": kind_of(&name_of(&p), false), "dir": false }))
        .collect();
    Ok(Json(json!({ "usage": usage(&app), "by_kind": by_kind, "trash": trash_bytes, "files": files, "largest": largest })))
}

// ---------- public share pages ----------
#[derive(Deserialize)]
pub struct PubQ {
    #[serde(default)]
    path: String,
    dl: Option<u8>,
}

async fn resolve_share(app: &Shared, id: &str, sub: &str) -> R<(Share, PathBuf, Vec<String>)> {
    let gone = || fail(StatusCode::NOT_FOUND, "This link doesn't work anymore. Ask the owner for a new one.");
    let s = app.meta.lock().await.shares.get(id).cloned().ok_or_else(gone)?;
    if s.expires.is_some_and(|e| e <= now()) {
        return Err(gone());
    }
    let sub = clean(sub)?;
    if !s.dir && !sub.is_empty() {
        return Err(gone());
    }
    let mut parts = clean(&s.path)?;
    parts.extend(sub.iter().cloned());
    let abs = join(&app.dirs.files, &parts);
    abs.symlink_metadata().map_err(|_| gone())?;
    Ok((s, abs, sub))
}

pub async fn pub_info(State(app): State<Shared>, AxPath(id): AxPath<String>, Query(q): Query<PubQ>) -> R<Json<Value>> {
    let (s, abs, sub) = resolve_share(&app, &id, &q.path).await?;
    let md = tokio::fs::metadata(&abs).await?;
    let root_name = name_of(&s.path);
    let mut v = json!({
        "name": root_name, "dir": md.is_dir(), "path": display(&sub), "expires": s.expires,
        "size": md.len(), "mtime": mtime(&md), "kind": kind_of(&root_name, md.is_dir()), "thumb": thumbable(&root_name),
    });
    if md.is_dir() {
        let none = HashSet::new();
        let mut out = Vec::new();
        let mut rd = tokio::fs::read_dir(&abs).await?;
        while let Some(e) = rd.next_entry().await? {
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(md) = e.metadata().await else { continue };
            let p = if sub.is_empty() { format!("/{name}") } else { format!("{}/{name}", display(&sub)) };
            out.push(entry(&name, &p, &md, &none, &none));
        }
        sort_entries(&mut out);
        v["entries"] = json!(out);
    }
    Ok(Json(v))
}

pub async fn pub_file(State(app): State<Shared>, AxPath(id): AxPath<String>, Query(q): Query<PubQ>, headers: HeaderMap) -> R<Response> {
    let (_, abs, _) = resolve_share(&app, &id, &q.path).await?;
    if q.dl == Some(1) && !headers.contains_key(header::RANGE) {
        let mut m = app.meta.lock().await;
        if let Some(s) = m.shares.get_mut(&id) {
            s.downloads += 1;
        }
        app.save(&m);
    }
    serve(abs, headers, q.dl == Some(1)).await
}

pub async fn pub_zip(State(app): State<Shared>, AxPath(id): AxPath<String>, Query(q): Query<PubQ>) -> R<Response> {
    let (s, abs, sub) = resolve_share(&app, &id, &q.path).await?;
    let name = sub.last().cloned().unwrap_or_else(|| name_of(&s.path));
    {
        let mut m = app.meta.lock().await;
        if let Some(s) = m.shares.get_mut(&id) {
            s.downloads += 1;
        }
        app.save(&m);
    }
    Ok(zip_response(vec![(abs, name.clone())], &format!("{name}.zip")))
}

pub async fn pub_thumb(State(app): State<Shared>, AxPath(id): AxPath<String>, Query(q): Query<PubQ>) -> R<Response> {
    let (s, abs, sub) = resolve_share(&app, &id, &q.path).await?;
    let key = format!("{}{}", s.path, display(&sub));
    thumb_of(&app, abs, &key).await
}

pub async fn me(_a: Authed, State(app): State<Shared>) -> Json<Value> {
    Json(json!({ "user": app.user, "usage": usage(&app) }))
}

pub fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not found").into_response()
}
