#!/bin/bash
set -euo pipefail

APP_NAME="${APP_NAME:-Vibe Kanban}"
BUNDLE_DIR="${BUNDLE_DIR:-target/release/bundle}"
APP_PATH="$BUNDLE_DIR/macos/$APP_NAME.app"
DMG_DIR="$BUNDLE_DIR/dmg"
DMG_PATH="$DMG_DIR/$APP_NAME.dmg"

echo "Freeing disk space before signing..."
df -h
rm -rf target/debug target/release/build target/release/deps target/release/examples target/release/incremental || true
rm -rf "$HOME/.cargo/registry" "$HOME/.cargo/git" "$HOME/.rustup/downloads" || true
df -h

if [ ! -d "$APP_PATH" ]; then
  echo "Error: $APP_PATH not found"
  exit 1
fi

echo "Ad-hoc signing $APP_NAME.app..."
codesign --force --deep --sign - "$APP_PATH"

mkdir -p "$DMG_DIR"
rm -f "$DMG_DIR"/*.dmg

echo "Recreating DMG with signed app..."
hdiutil create -volname "$APP_NAME" -srcfolder "$APP_PATH" -ov -format UDZO "$DMG_PATH"

echo "Done: $DMG_PATH"
