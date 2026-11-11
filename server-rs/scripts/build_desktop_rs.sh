#!/usr/bin/env bash
# Build the Rust portable desktop app (Tauri shell + axum server, M4).
#
# Produces dist/mem0-desktop-<os>-<arch>.tar.gz containing the single binary and a
# README. The dashboard export (server/dashboard/out) is embedded into the binary
# at build time via tauri.conf.json frontendDist, so no Node is needed at runtime.
#
# Usage:
#   server-rs/scripts/build_desktop_rs.sh            # release build + tar.gz
#   server-rs/scripts/build_desktop_rs.sh --debug    # debug build (faster, bigger)
#   server-rs/scripts/build_desktop_rs.sh --skip-dashboard-build
#         # reuse the existing server/dashboard/out instead of running pnpm build
#
# Tauri cannot cross-compile: run this on the platform you are shipping to.
# CI (.github/workflows/desktop-build-rs.yml) runs it on ubuntu/windows/macos runners.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SERVER_RS="$REPO_ROOT/server-rs"
DASHBOARD_DIR="$REPO_ROOT/server/dashboard"
DIST="$REPO_ROOT/dist"
PROFILE="release"
SKIP_DASHBOARD=0

for arg in "$@"; do
  case "$arg" in
    --debug) PROFILE="debug" ;;
    --skip-dashboard-build) SKIP_DASHBOARD=1 ;;
    *) echo "unknown arg: $arg" >&2; exit 2 ;;
  esac
done

OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in
  Linux)  OS_TAG="linux" ;;
  Darwin) OS_TAG="macos" ;;
  MINGW*|MSYS*|CYGWIN*) OS_TAG="windows" ;;
  *) echo "unsupported OS: $OS" >&2; exit 2 ;;
esac
case "$ARCH" in
  x86_64|amd64) ARCH_TAG="x64" ;;
  aarch64|arm64) ARCH_TAG="arm64" ;;
  *) ARCH_TAG="$ARCH" ;;
esac

# 1. Dashboard export (must exist before tauri build; it is embedded into the binary).
if [ "$SKIP_DASHBOARD" -eq 0 ]; then
  if [ ! -f "$DASHBOARD_DIR/out/index.html" ]; then
    echo ">> building dashboard export (pnpm)"
    (cd "$DASHBOARD_DIR" && pnpm install --frozen-lockfile && MEM0_DASHBOARD_OUTPUT=export pnpm build)
  else
    echo ">> reusing existing $DASHBOARD_DIR/out"
  fi
fi
[ -f "$DASHBOARD_DIR/out/index.html" ] || { echo "dashboard export missing" >&2; exit 1; }

# 2. Tauri release build.
echo ">> cargo build -p mem0-desktop --$PROFILE"
(cd "$SERVER_RS" && cargo build -p mem0-desktop --"$PROFILE")

BIN="$SERVER_RS/target/$PROFILE/mem0-desktop"
[ -x "$BIN" ] || { echo "binary not found: $BIN" >&2; exit 1; }

# 3. Stage + tar.gz. The archive contains the binary and README only — never data/.
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/mem0-desktop"
cp "$BIN" "$STAGE/mem0-desktop/mem0-desktop"
chmod +x "$STAGE/mem0-desktop/mem0-desktop"
cp "$SERVER_RS/crates/desktop/README.md" "$STAGE/mem0-desktop/README.md"

mkdir -p "$DIST"
# "-rs" keeps this distinguishable from the legacy PyInstaller artifacts of the same
# platform tag; getting them confused installs a build that needs pythonXY.dll.
OUT="$DIST/mem0-desktop-${OS_TAG}-${ARCH_TAG}-rs.tar.gz"
rm -f "$OUT"
tar -czf "$OUT" -C "$STAGE" mem0-desktop
echo ">> wrote $OUT ($(du -h "$OUT" | cut -f1))"
sha256sum "$OUT" | awk '{print ">> sha256: " $1}'
