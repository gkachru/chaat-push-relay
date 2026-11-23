#!/usr/bin/env bash
# Builds dist/chaat-push-relay_<version>-1_amd64.deb for Debian 12+ and Ubuntu 22.04+ (glibc 2.35).
# Needs cargo-deb, cargo-zigbuild and zig.
set -euo pipefail
cd "$(dirname "$0")/.."
target=x86_64-unknown-linux-gnu
cargo zigbuild --release --locked --target "$target.2.35" --bin chaat-push-relay
cargo deb --no-build --target "$target" --output dist/
ls -l dist/*.deb
