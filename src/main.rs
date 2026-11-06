//! MyDrive — a tiny, single-binary personal cloud drive.
//! Files live in a plain folder on disk; the web app is embedded.

mod api;
mod media;
mod store;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, FromRequestParts, Request, State};
use axum::http::{header, request::Parts, HeaderValue, Method, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{any, get, post, put};
use axum::{Json, Router};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD as B64};
use base64::Engine;
use dav_server::{fakels::FakeLs, localfs::LocalFs, DavHandler};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use store::{fail, now, tree_size, Dirs, Meta, R};

const INDEX: &str = include_str!("index.html");
const SHARE_PAGE: &str = include_str!("share.html");
const SESSION_DAYS: u64 = 30;
const TRASH_DAYS: u64 = 30;

pub struct App {
    pub dirs: Dirs,
    pub quota: u64,
    pub used: AtomicU64,
    key: Vec<u8>,
    pub user: String,
    pw_hash: String,
    secure_cookie: bool,
    fails: Mutex<VecDeque<Instant>>,
    pub meta: tokio::sync::Mutex<Meta>,
    pub thumb_gate: tokio::sync::Semaphore,
    dav: DavHandler,
    dav_ok: Mutex<HashMap<[u8; 32], Instant>>,
}
pub type Shared = Arc<App>;

impl App {
    pub fn free(&self) -> u64 {
        self.quota.saturating_sub(self.used.load(Ordering::Relaxed))
    }
    pub fn save(&self, m: &Meta) {
        if let Err(e) = m.save(&self.dirs.state.join("meta.json")) {
            eprintln!("could not save metadata: {e}");
        }
    }
}

async fn recount(app: &Shared) {
    let (f, t) = (app.dirs.files.clone(), app.dirs.trash.clone());
    if let Ok(n) = tokio::task::spawn_blocking(move || tree_size(&f) + tree_size(&t)).await {
        app.used.store(n, Ordering::Relaxed);
    }
}

// ---------- sessions ----------
fn mac(key: &[u8], payload: &str) -> Hmac<Sha256> {
    let mut m = <Hmac<Sha256>>::new_from_slice(key).expect("hmac key");
    m.update(payload.as_bytes());
    m
}

fn make_token(key: &[u8], payload: &str) -> String {
    format!("{}.{}", B64.encode(payload), B64.encode(mac(key, payload).finalize().into_bytes()))
}

fn verify_token(key: &[u8], token: &str) -> Option<String> {
    let (p, s) = token.split_once('.')?;
    let payload = String::from_utf8(B64.decode(p).ok()?).ok()?;
    mac(key, &payload).verify_slice(&B64.decode(s).ok()?).ok()?;
    Some(payload)
}

pub struct Authed;

impl FromRequestParts<Shared> for Authed {
    type Rejection = store::AppError;
    async fn from_request_parts(parts: &mut Parts, app: &Shared) -> Result<Self, Self::Rejection> {
        let unauth = || fail(StatusCode::UNAUTHORIZED, "Please log in");
        let token = parts
            .headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .find_map(|kv| kv.trim().strip_prefix("md_session="))
            .ok_or_else(unauth)?;
        let payload = verify_token(&app.key, token).ok_or_else(unauth)?;
        let mut it = payload.splitn(3, '|');
        let (kind, user, exp) = (it.next(), it.next(), it.next().and_then(|e| e.parse::<u64>().ok()).unwrap_or(0));
        if kind != Some("s") || user != Some(app.user.as_str()) || exp < now() {
            return Err(unauth());
        }
        // CSRF guard: state-changing calls must carry our custom header.
        if parts.method != Method::GET && parts.method != Method::HEAD && !parts.headers.contains_key("x-mydrive") {
            return Err(fail(StatusCode::FORBIDDEN, "Missing X-MyDrive header"));
        }
        Ok(Authed)
    }
}

/// Shared password check with lockout (web login and WebDAV).
async fn check_password(app: &Shared, user: &str, password: String) -> R<()> {
    {
        let mut f = app.fails.lock().unwrap();
        while f.front().is_some_and(|t| t.elapsed() > Duration::from_secs(900)) {
            f.pop_front();
        }
        if f.len() >= 10 {
            return Err(fail(StatusCode::TOO_MANY_REQUESTS, "Too many failed logins. Try again in 15 minutes."));
        }
    }
    let hash = app.pw_hash.clone();
    let ok = tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash).map(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok()).unwrap_or(false)
    })
    .await
    .unwrap_or(false);
    if ok && user.trim().eq_ignore_ascii_case(&app.user) {
        return Ok(());
    }
    app.fails.lock().unwrap().push_back(Instant::now());
    tokio::time::sleep(Duration::from_secs(1)).await;
    Err(fail(StatusCode::UNAUTHORIZED, "Wrong username or password"))
}

fn cookie(app: &App, value: &str, max_age: u64) -> HeaderValue {
    let secure = if app.secure_cookie { "; Secure" } else { "" };
    HeaderValue::from_str(&format!("md_session={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}"))
        .expect("cookie header")
}

#[derive(Deserialize)]
struct LoginBody {
    user: String,
    password: String,
}

async fn login(State(app): State<Shared>, Json(b): Json<LoginBody>) -> R<Response> {
    check_password(&app, &b.user, b.password).await?;
    let exp = now() + SESSION_DAYS * 86400;
    let token = make_token(&app.key, &format!("s|{}|{exp}", app.user));
    let mut res = Json(json!({ "user": app.user })).into_response();
    res.headers_mut().insert(header::SET_COOKIE, cookie(&app, &token, SESSION_DAYS * 86400));
    Ok(res)
}

async fn logout(State(app): State<Shared>) -> Response {
    let mut res = Json(json!({ "ok": true })).into_response();
    res.headers_mut().insert(header::SET_COOKIE, cookie(&app, "", 0));
    res
}

// ---------- WebDAV (map as a drive in Windows / macOS) ----------
async fn dav_route(State(app): State<Shared>, req: Request) -> Response {
    let challenge = || {
        let mut r = (StatusCode::UNAUTHORIZED, "Login required").into_response();
        r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"MyDrive\", charset=\"UTF-8\""));
        r
    };
    let Some(auth) = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).map(str::to_string) else {
        return challenge();
    };
    let digest: [u8; 32] = Sha256::digest(format!("{}|{auth}", B64.encode(&app.key)).as_bytes()).into();
    let cached = app.dav_ok.lock().unwrap().get(&digest).is_some_and(|t| t.elapsed() < Duration::from_secs(600));
    if !cached {
        let creds = auth
            .strip_prefix("Basic ")
            .and_then(|b| STANDARD.decode(b.trim()).ok())
            .and_then(|b| String::from_utf8(b).ok());
        let Some((user, pw)) = creds.as_deref().and_then(|c| c.split_once(':')) else { return challenge() };
        if check_password(&app, user, pw.to_string()).await.is_err() {
            return challenge();
        }
        let mut c = app.dav_ok.lock().unwrap();
        c.retain(|_, t| t.elapsed() < Duration::from_secs(600));
        c.insert(digest, Instant::now());
    }

    let rel = percent_encoding::percent_decode_str(req.uri().path().strip_prefix("/dav").unwrap_or("")).decode_utf8_lossy().to_string();
    // Deleting from Explorer goes to Trash, same as the web app.
    if req.method() == Method::DELETE {
        return match store::clean(&rel) {
            Ok(parts) if !parts.is_empty() => match api::trash_one(&app, &parts).await {
                Ok(_) => StatusCode::NO_CONTENT.into_response(),
                Err(e) => e.into_response(),
            },
            _ => StatusCode::FORBIDDEN.into_response(),
        };
    }
    let is_put = req.method() == Method::PUT;
    if is_put {
        let len = req.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        if len > app.free() || app.free() == 0 {
            return (StatusCode::INSUFFICIENT_STORAGE, "Drive is full").into_response();
        }
    }
    let is_copy = req.method().as_str() == "COPY";
    let res = app.dav.handle(req).await;
    if (is_put || is_copy) && res.status().is_success() {
        let app = app.clone();
        tokio::spawn(async move { recount(&app).await });
    }
    res.map(Body::new)
}

// ---------- pages ----------
fn page(html: &'static str) -> Response {
    let mut res = Html(html).into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; media-src 'self'; frame-src 'self'; frame-ancestors 'self'",
        ),
    );
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    res
}

// ---------- startup ----------
fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(data)
}

fn set_password(state: &Path) {
    let mut pw = String::new();
    std::io::stdin().read_line(&mut pw).expect("read password");
    let pw = pw.trim_end_matches(['\r', '\n']);
    if pw.len() < 8 {
        eprintln!("Password must be at least 8 characters");
        std::process::exit(1);
    }
    let mut salt = [0u8; 16];
    getrandom::getrandom(&mut salt).expect("randomness");
    let salt = SaltString::encode_b64(&salt).expect("salt");
    let hash = Argon2::default().hash_password(pw.as_bytes(), &salt).expect("hash").to_string();
    std::fs::create_dir_all(state).expect("state dir");
    write_private(&state.join("password"), hash.as_bytes()).expect("write password");
    println!("Password saved.");
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let data = PathBuf::from(env("MYDRIVE_DATA", "./data"));
    let dirs = Dirs::new(&data);
    if std::env::args().nth(1).as_deref() == Some("set-password") {
        set_password(&dirs.state);
        return;
    }
    dirs.create().expect("create data folders");
    let quota_gb: f64 = env("MYDRIVE_QUOTA_GB", "40").parse().expect("MYDRIVE_QUOTA_GB");
    let listen = env("MYDRIVE_LISTEN", "127.0.0.1:8090");

    let key_path = dirs.state.join("secret");
    let key = match std::fs::read(&key_path) {
        Ok(k) if k.len() >= 32 => k,
        _ => {
            let mut k = vec![0u8; 32];
            getrandom::getrandom(&mut k).expect("randomness");
            write_private(&key_path, &k).expect("write secret");
            k
        }
    };
    let pw_hash = std::fs::read_to_string(dirs.state.join("password")).unwrap_or_else(|_| {
        eprintln!("No password set. Run: echo 'your-password' | mydrive set-password");
        std::process::exit(1);
    });
    let meta = Meta::load(&dirs.state.join("meta.json"));
    let dav_handler = DavHandler::builder()
        .filesystem(LocalFs::new(&dirs.files, false, false, false))
        .locksystem(FakeLs::new())
        .strip_prefix("/dav")
        .hide_symlinks(true)
        .build_handler();

    let app = Arc::new(App {
        dirs,
        quota: (quota_gb * 1024.0 * 1024.0 * 1024.0) as u64,
        used: AtomicU64::new(0),
        key,
        user: env("MYDRIVE_USER", "sreekar"),
        pw_hash: pw_hash.trim().to_string(),
        secure_cookie: env("MYDRIVE_INSECURE_COOKIE", "0") != "1",
        fails: Mutex::new(VecDeque::new()),
        meta: tokio::sync::Mutex::new(meta),
        thumb_gate: tokio::sync::Semaphore::new(1),
        dav: dav_handler,
        dav_ok: Mutex::new(HashMap::new()),
    });
    recount(&app).await;

    // housekeeping: recount usage, auto-empty Trash after 30 days
    {
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                let old: Vec<String> = {
                    let m = app.meta.lock().await;
                    m.trash.iter().filter(|(_, t)| t.deleted + TRASH_DAYS * 86400 < now()).map(|(k, _)| k.clone()).collect()
                };
                if !old.is_empty() {
                    let _ = api::purge(&app, &old).await;
                }
                recount(&app).await;
                tokio::time::sleep(Duration::from_secs(900)).await;
            }
        });
    }

    let router = Router::new()
        .route("/", get(|| async { page(INDEX) }))
        .route("/s/{id}", get(|| async { page(SHARE_PAGE) }))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/me", get(api::me))
        .route("/api/list", get(api::list))
        .route("/api/recent", get(api::recent))
        .route("/api/search", get(api::search))
        .route("/api/starred", get(api::starred))
        .route("/api/info", get(api::info))
        .route("/api/mkdir", post(api::mkdir))
        .route("/api/rename", post(api::rename))
        .route("/api/move", post(api::mv))
        .route("/api/copy", post(api::copy))
        .route("/api/star", post(api::star))
        .route("/api/delete", post(api::delete))
        .route("/api/trash", get(api::trash_list))
        .route("/api/trash/restore", post(api::trash_restore))
        .route("/api/trash/delete", post(api::trash_delete))
        .route("/api/trash/empty", post(api::trash_empty))
        .route("/api/upload", put(api::upload).layer(DefaultBodyLimit::disable()))
        .route("/api/file", get(api::file))
        .route("/api/thumb", get(api::thumb))
        .route("/api/zip", get(api::zip))
        .route("/api/share", post(api::share))
        .route("/api/shares", get(api::shares))
        .route("/api/share/revoke", post(api::unshare))
        .route("/api/storage", get(api::storage))
        .route("/api/public/{id}", get(api::pub_info))
        .route("/api/public/{id}/file", get(api::pub_file))
        .route("/api/public/{id}/zip", get(api::pub_zip))
        .route("/api/public/{id}/thumb", get(api::pub_thumb))
        .route("/dav", any(dav_route).layer(DefaultBodyLimit::disable()))
        .route("/dav/", any(dav_route).layer(DefaultBodyLimit::disable()))
        .route("/dav/{*rest}", any(dav_route).layer(DefaultBodyLimit::disable()))
        .fallback(|| async { api::not_found() })
        .with_state(app.clone());

    let listener = tokio::net::TcpListener::bind(&listen).await.expect("bind");
    println!("MyDrive listening on {listen} — {} bytes of {quota_gb} GB used", app.used.load(Ordering::Relaxed));
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal");
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
        })
        .await
        .expect("server");
}
