# MyDrive

A tiny self-hosted personal cloud drive — one Rust binary (~3 MB, ~45 MB RAM) with an embedded web app.

- Google-Drive-style web UI: My Drive, Recent, Starred, Shared links, Trash, Storage, search, grid/list, previews, details panel
- Uploads (files and folders, drag & drop, replace/keep both), ZIP downloads, share links (files and folders, expiring), Trash with 30-day auto-empty
- WebDAV at `/dav` so it can be mapped as a drive letter in Windows / Finder
- Files are stored as plain files on disk under `$MYDRIVE_DATA/files`

## Run

```
echo 'your-password' | MYDRIVE_DATA=/srv/mydrive mydrive set-password
MYDRIVE_DATA=/srv/mydrive MYDRIVE_LISTEN=127.0.0.1:8090 MYDRIVE_QUOTA_GB=40 MYDRIVE_USER=sreekar mydrive
```

Put it behind an HTTPS reverse proxy (cookies are `Secure`). See `deploy/` for the scripts used on OCI.

## Build for x86_64 Linux (static)

```
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```
