#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)"
if [[ -z "$VERSION" ]]; then
  echo "Failed to detect version from Cargo.toml" >&2
  exit 1
fi

TARGET="x86_64-pc-windows-msvc"
BUILD_EXE_NAME="logcatx.exe"
EXE_NAME="LogcatX.exe"
ZIP_NAME="LogcatX-v${VERSION}-win64.zip"
DIST_DIR="$ROOT/dist"
PACKAGE_DIR="$DIST_DIR/LogcatX-v${VERSION}-win64"

cargo xwin build --target "$TARGET" --release

rm -rf "$PACKAGE_DIR"
mkdir -p "$PACKAGE_DIR"
mkdir -p "$PACKAGE_DIR/icons"

cp "$ROOT/target/$TARGET/release/$BUILD_EXE_NAME" "$DIST_DIR/$EXE_NAME"
cp "$ROOT/target/$TARGET/release/$BUILD_EXE_NAME" "$PACKAGE_DIR/$EXE_NAME"
cp "$ROOT/README.md" "$PACKAGE_DIR/README.md"
cp "$ROOT/README.en.md" "$PACKAGE_DIR/README.en.md"
cp "$ROOT/CHANGELOG.md" "$PACKAGE_DIR/CHANGELOG.md"
cp "$ROOT/LICENSE" "$PACKAGE_DIR/LICENSE"
cp "$ROOT/config.example.json" "$PACKAGE_DIR/config.example.json"
cp "$ROOT/icons/icon_128.png" "$PACKAGE_DIR/icons/icon_128.png"

rm -f "$DIST_DIR/$ZIP_NAME"
python3 - "$DIST_DIR" "$ZIP_NAME" "$(basename "$PACKAGE_DIR")" <<'PY'
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
            rel_path = os.path.relpath(full_path, dist_dir)
            zf.write(full_path, rel_path)
PY

echo "Packaged release:"
echo "  $DIST_DIR/$EXE_NAME"
echo "  $DIST_DIR/$ZIP_NAME"
