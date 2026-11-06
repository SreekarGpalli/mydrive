//! File serving, streamed ZIP downloads and image thumbnails.

use crate::store::{civil, fail, mtime, R};
use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Request, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use std::io::Write;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tower::ServiceExt;
use tower_http::services::ServeFile;

/// Types a browser could execute — never render these inline.
fn risky(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(ext.as_str(), "html" | "htm" | "xhtml" | "svg" | "svgz" | "xml" | "js" | "mjs" | "shtml" | "xht")
}

pub fn disposition(attach: bool, name: &str) -> HeaderValue {
    let v = format!(
        "{}; filename*=UTF-8''{}",
        if attach { "attachment" } else { "inline" },
        utf8_percent_encode(name, NON_ALPHANUMERIC)
    );
    HeaderValue::from_str(&v).unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

/// Serve a file with Range support (resumable downloads, video seeking).
pub async fn serve(p: PathBuf, headers: HeaderMap, attach: bool) -> R<Response> {
    let md = tokio::fs::metadata(&p).await?;
    if !md.is_file() {
        return Err(fail(StatusCode::NOT_FOUND, "Not a file"));
    }
    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let danger = risky(&name);
    let mut req = Request::new(Body::empty());
    for k in [header::RANGE, header::IF_RANGE, header::IF_MODIFIED_SINCE, header::IF_NONE_MATCH] {
        if let Some(v) = headers.get(&k) {
            req.headers_mut().insert(k, v.clone());
        }
    }
    let res = ServeFile::new(&p)
        .oneshot(req)
        .await
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut res = res.map(Body::new);
    let h = res.headers_mut();
    h.insert(header::CONTENT_DISPOSITION, disposition(attach || danger, &name));
    if danger {
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
        h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox"));
    }
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    Ok(res)
}

// ---------- ZIP ----------
struct ChanWriter {
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
    buf: Vec<u8>,
}

impl ChanWriter {
    fn push(&mut self) -> std::io::Result<()> {
        if !self.buf.is_empty() {
            let chunk = Bytes::from(std::mem::take(&mut self.buf));
            self.tx
                .blocking_send(Ok(chunk))
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "download cancelled"))?;
            self.buf.reserve(256 * 1024);
        }
        Ok(())
    }
}

impl Write for ChanWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= 256 * 1024 {
            self.push()?;
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.push()
    }
}

impl Drop for ChanWriter {
    fn drop(&mut self) {
        let _ = self.push();
    }
}

fn zip_opts(md: &std::fs::Metadata) -> zip::write::SimpleFileOptions {
    let (y, mo, d, h, mi, s) = civil(mtime(md));
    let mut o = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .large_file(md.len() >= 0xFFFF_0000);
    if let Ok(dt) = zip::DateTime::from_date_and_time(y.max(1980), mo, d, h, mi, s) {
        o = o.last_modified_time(dt);
    }
    o
}

fn add<W: Write>(z: &mut zip::ZipWriter<zip::write::StreamWriter<W>>, abs: &Path, name: &str) -> zip::result::ZipResult<()> {
    let md = abs.symlink_metadata()?;
    if md.is_dir() {
        z.add_directory(format!("{name}/"), zip_opts(&md))?;
        let mut kids: Vec<_> = std::fs::read_dir(abs)?.flatten().collect();
        kids.sort_by_key(|e| e.file_name());
        for e in kids {
            add(z, &e.path(), &format!("{name}/{}", e.file_name().to_string_lossy()))?;
        }
    } else if md.is_file() {
        z.start_file(name, zip_opts(&md))?;
        let mut f = std::fs::File::open(abs)?;
        std::io::copy(&mut f, z)?;
    }
    Ok(())
}

/// Stream a ZIP of the given (absolute path, name inside zip) items.
pub fn zip_response(items: Vec<(PathBuf, String)>, filename: &str) -> Response {
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    tokio::task::spawn_blocking(move || {
        let w = ChanWriter { tx: tx.clone(), buf: Vec::with_capacity(256 * 1024) };
        let mut z = zip::ZipWriter::new_stream(w);
        let res = (|| {
            for (abs, name) in &items {
                add(&mut z, abs, name)?;
            }
            z.finish()?;
            Ok::<_, zip::result::ZipError>(())
        })();
        if let Err(e) = res {
            let _ = tx.blocking_send(Err(std::io::Error::other(e.to_string())));
        }
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
    let mut res = Response::new(Body::from_stream(stream));
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/zip"));
    h.insert(header::CONTENT_DISPOSITION, disposition(true, filename));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

// ---------- thumbnails ----------
pub fn thumbable(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp")
}

pub fn make_thumb(src: &Path, dst: &Path) -> Result<(), String> {
    use image::ImageDecoder;
    let mut dec = image::ImageReader::open(src)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .into_decoder()
        .map_err(|e| e.to_string())?;
    let orientation = dec.orientation().ok();
    let mut img = image::DynamicImage::from_decoder(dec).map_err(|e| e.to_string())?;
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }
    let rgb = img.thumbnail(480, 480).to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 78)
        .encode_image(&rgb)
        .map_err(|e| e.to_string())?;
    let tmp = dst.with_extension("part");
    std::fs::write(&tmp, out).map_err(|e| e.to_string())?;
    std::fs::rename(tmp, dst).map_err(|e| e.to_string())
}
