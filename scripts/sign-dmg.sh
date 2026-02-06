#!/bin/bash
set -e

APP_NAME="Vibe Kanban"
BUNDLE_DIR="target/release/bundle"
APP_PATH="$BUNDLE_DIR/macos/$APP_NAME.app"
DMG_DIR="$BUNDLE_DIR/dmg"

if [ ! -d "$APP_PATH" ]; then
  echo "Error: $APP_PATH not found"
  exit 1
fi

echo "Ad-hoc signing $APP_NAME.app..."
codesign --force --deep --sign - "$APP_PATH"

echo "Recreating DMG with signed app..."
rm -f "$DMG_DIR"/*.dmg
hdiutil create -volname "$APP_NAME" -srcfolder "$APP_PATH" -ov -format UDZO "$DMG_DIR/$APP_NAME.dmg"

echo "Done: $DMG_DIR/$APP_NAME.dmg"
