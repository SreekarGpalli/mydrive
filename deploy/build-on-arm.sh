#!/bin/bash
# Runs as ubuntu on arm1: build MyDrive for the x86_64 Micro servers.
set -e
exec > /tmp/mydrive-build.log 2>&1
rm -rf ~/mydrive-src && mkdir -p ~/mydrive-src && tar xJf /tmp/mydrive-src.txz -C ~/mydrive-src
if [ ! -x ~/.cargo/bin/cargo ]; then curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path; fi
~/.cargo/bin/rustup target add x86_64-unknown-linux-musl
cd ~/mydrive-src && ~/.cargo/bin/cargo build --release --target x86_64-unknown-linux-musl
ls -la target/x86_64-unknown-linux-musl/release/mydrive
file target/x86_64-unknown-linux-musl/release/mydrive || true
echo BUILD_DONE
