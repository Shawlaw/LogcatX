#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)"
if [[ -z "$VERSION" ]]; then
  echo "Failed to detect version from Cargo.toml" >&2
  exit 1
fi

COMMIT_ID="$(git rev-parse --short=7 HEAD 2>/dev/null || echo unknown)"
TARGET="x86_64-pc-windows-msvc"
BUILD_EXE_NAME="logcatx.exe"
HELPER_BUILD_EXE_NAME="logcatx-updater.exe"
EXE_NAME="LogcatX.exe"
HELPER_EXE_NAME="LogcatX.Updater.exe"
ZIP_NAME="LogcatX_${VERSION}_windows_x64_portable_${COMMIT_ID}.zip"
DIST_DIR="$ROOT/dist"
PACKAGE_DIR="$DIST_DIR/LogcatX-${VERSION}-win64"

# Native Windows hosts build directly; other hosts cross-compile with cargo-xwin.
OS_NAME="$(uname -s)"
case "$OS_NAME" in
  MINGW*|MSYS*|CYGWIN*)
    cargo build --release
    RELEASE_DIR="$ROOT/target/release"
    ;;
  *)
    cargo xwin build --target "$TARGET" --release
    RELEASE_DIR="$ROOT/target/$TARGET/release"
    ;;
esac

rm -rf "$PACKAGE_DIR"
mkdir -p "$PACKAGE_DIR"
mkdir -p "$PACKAGE_DIR/icons"

cp "$RELEASE_DIR/$BUILD_EXE_NAME" "$DIST_DIR/$EXE_NAME"
cp "$RELEASE_DIR/$BUILD_EXE_NAME" "$PACKAGE_DIR/$EXE_NAME"
cp "$RELEASE_DIR/$HELPER_BUILD_EXE_NAME" "$PACKAGE_DIR/$HELPER_EXE_NAME"
cp "$ROOT/README.md" "$PACKAGE_DIR/README.md"
cp "$ROOT/README.en.md" "$PACKAGE_DIR/README.en.md"
cp "$ROOT/CHANGELOG.md" "$PACKAGE_DIR/CHANGELOG.md"
cp "$ROOT/CHANGELOG.en.md" "$PACKAGE_DIR/CHANGELOG.en.md"
cp "$ROOT/LICENSE" "$PACKAGE_DIR/LICENSE"
cp "$ROOT/config.example.json" "$PACKAGE_DIR/config.example.json"
cp "$ROOT/icons/icon_128.png" "$PACKAGE_DIR/icons/icon_128.png"

# Flat portable layout: archive entries sit at the ZIP root and must match the
# allow-list in desktop-update.toml exactly.
rm -f "$DIST_DIR/$ZIP_NAME"
PYTHON_BIN=""
for candidate in python3 python py; do
  if command -v "$candidate" >/dev/null 2>&1 && "$candidate" -c "print(1)" >/dev/null 2>&1; then
    PYTHON_BIN="$candidate"
    break
  fi
done
if [[ -z "$PYTHON_BIN" ]]; then
  echo "No usable Python interpreter found (tried python3, python, py)" >&2
  exit 1
fi
"$PYTHON_BIN" - "$DIST_DIR" "$ZIP_NAME" "$(basename "$PACKAGE_DIR")" <<'PY'
import os
import sys
import zipfile

dist_dir, zip_name, package_dir_name = sys.argv[1:4]
zip_path = os.path.join(dist_dir, zip_name)
package_root = os.path.join(dist_dir, package_dir_name)

with zipfile.ZipFile(zip_path, "w", compression=zipfile.ZIP_DEFLATED) as zf:
    for root, _, files in os.walk(package_root):
        for file_name in files:
            full_path = os.path.join(root, file_name)
            rel_path = os.path.relpath(full_path, package_root)
            zf.write(full_path, rel_path)
PY

echo "Packaged release:"
echo "  $DIST_DIR/$EXE_NAME"
echo "  $DIST_DIR/$ZIP_NAME"
