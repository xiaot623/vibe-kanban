#!/bin/bash

set -e  # Exit on any error

# Available platforms
ALL_PLATFORMS=("linux-x64" "linux-arm64" "macos-x64" "macos-arm64" "windows-x64")

show_help() {
  echo "Usage: $0 [OPTIONS] [PLATFORMS...]"
  echo ""
  echo "Cross-compile vibe-kanban for multiple platforms using Docker."
  echo ""
  echo "Options:"
  echo "  -h, --help     Show this help message"
  echo "  -l, --list     List available platforms"
  echo "  -a, --all      Build for all platforms"
  echo ""
  echo "Platforms:"
  echo "  linux-x64      Linux x86_64"
  echo "  linux-arm64    Linux ARM64"
  echo "  macos-x64      macOS x86_64 (requires osxcross SDK)"
  echo "  macos-arm64    macOS ARM64 (requires osxcross SDK)"
  echo "  windows-x64    Windows x86_64"
  echo ""
  echo "Examples:"
  echo "  $0 linux-x64 linux-arm64    # Build for Linux only"
  echo "  $0 windows-x64              # Build for Windows only"
  echo "  $0 --all                    # Build for all platforms"
}

list_platforms() {
  echo "Available platforms:"
  for p in "${ALL_PLATFORMS[@]}"; do
    echo "  - $p"
  done
}

# Parse arguments
PLATFORMS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help)
      show_help
      exit 0
      ;;
    -l|--list)
      list_platforms
      exit 0
      ;;
    -a|--all)
      PLATFORMS=("${ALL_PLATFORMS[@]}")
      shift
      ;;
    -*)
      echo "❌ Unknown option: $1"
      show_help
      exit 1
      ;;
    *)
      PLATFORMS+=("$1")
      shift
      ;;
  esac
done

# Default to showing help if no platforms specified
if [ ${#PLATFORMS[@]} -eq 0 ]; then
  show_help
  exit 1
fi

# Validate platforms
for PLATFORM in "${PLATFORMS[@]}"; do
  VALID=false
  for P in "${ALL_PLATFORMS[@]}"; do
    if [ "$PLATFORM" == "$P" ]; then
      VALID=true
      break
    fi
  done
  if [ "$VALID" == "false" ]; then
    echo "❌ Invalid platform: $PLATFORM"
    list_platforms
    exit 1
  fi
done

echo "🎯 Building for: ${PLATFORMS[*]}"
echo ""

echo "🧹 Cleaning previous builds..."
rm -rf artifact
mkdir -p artifact

echo "🔨 Building frontend..."
(cd frontend && npm run build)

# Build for each platform
for PLATFORM in "${PLATFORMS[@]}"; do
  echo ""
  echo "=========================================="
  echo "🐳 Building for $PLATFORM..."
  echo "=========================================="

  mkdir -p artifact/$PLATFORM
  BINARY_NAME="vibe-kanban"

  case "$PLATFORM" in
    linux-x64)
      DOCKER_PLATFORM="linux/amd64"
      RUST_TARGET="x86_64-unknown-linux-gnu"
      docker run --rm \
        --platform "$DOCKER_PLATFORM" \
        -v "$(pwd)":/workspace \
        -w /workspace \
        rust:1.83-bookworm \
        bash -c "
          cargo build --release --bin server --manifest-path Cargo.toml
          cp /workspace/target/release/server /workspace/artifact/$PLATFORM/$BINARY_NAME
        "
      ;;

    linux-arm64)
      DOCKER_PLATFORM="linux/arm64"
      RUST_TARGET="aarch64-unknown-linux-gnu"
      docker run --rm \
        --platform "$DOCKER_PLATFORM" \
        -v "$(pwd)":/workspace \
        -w /workspace \
        rust:1.83-bookworm \
        bash -c "
          cargo build --release --bin server --manifest-path Cargo.toml
          cp /workspace/target/release/server /workspace/artifact/$PLATFORM/$BINARY_NAME
        "
      ;;

    windows-x64)
      RUST_TARGET="x86_64-pc-windows-gnu"
      BINARY_NAME="vibe-kanban.exe"
      docker run --rm \
        --platform "linux/amd64" \
        -v "$(pwd)":/workspace \
        -w /workspace \
        rust:1.83-bookworm \
        bash -c "
          apt-get update && apt-get install -y gcc-mingw-w64-x86-64 >/dev/null 2>&1
          rustup target add $RUST_TARGET
          cargo build --release --bin server --manifest-path Cargo.toml --target $RUST_TARGET
          cp /workspace/target/$RUST_TARGET/release/server.exe /workspace/artifact/$PLATFORM/$BINARY_NAME
        "
      ;;

    macos-x64)
      RUST_TARGET="x86_64-apple-darwin"
      # macOS cross-compilation requires osxcross with Apple SDK
      # Check if OSXCROSS_SDK_PATH is set
      if [ -z "$OSXCROSS_SDK_PATH" ]; then
        echo "⚠️  macOS cross-compilation requires osxcross."
        echo "   Set OSXCROSS_SDK_PATH to your SDK path, or build natively on macOS."
        echo "   Skipping $PLATFORM..."
        continue
      fi
      docker run --rm \
        --platform "linux/amd64" \
        -v "$(pwd)":/workspace \
        -v "$OSXCROSS_SDK_PATH":/osxcross \
        -w /workspace \
        -e PATH="/osxcross/bin:$PATH" \
        -e CC="o64-clang" \
        -e CXX="o64-clang++" \
        rust:1.83-bookworm \
        bash -c "
          rustup target add $RUST_TARGET
          cargo build --release --bin server --manifest-path Cargo.toml --target $RUST_TARGET
          cp /workspace/target/$RUST_TARGET/release/server /workspace/artifact/$PLATFORM/$BINARY_NAME
        "
      ;;

    macos-arm64)
      RUST_TARGET="aarch64-apple-darwin"
      if [ -z "$OSXCROSS_SDK_PATH" ]; then
        echo "⚠️  macOS cross-compilation requires osxcross."
        echo "   Set OSXCROSS_SDK_PATH to your SDK path, or build natively on macOS."
        echo "   Skipping $PLATFORM..."
        continue
      fi
      docker run --rm \
        --platform "linux/amd64" \
        -v "$(pwd)":/workspace \
        -v "$OSXCROSS_SDK_PATH":/osxcross \
        -w /workspace \
        -e PATH="/osxcross/bin:$PATH" \
        -e CC="oa64-clang" \
        -e CXX="oa64-clang++" \
        rust:1.83-bookworm \
        bash -c "
          rustup target add $RUST_TARGET
          cargo build --release --bin server --manifest-path Cargo.toml --target $RUST_TARGET
          cp /workspace/target/$RUST_TARGET/release/server /workspace/artifact/$PLATFORM/$BINARY_NAME
        "
      ;;

    *)
      echo "❌ Unknown platform: $PLATFORM"
      exit 1
      ;;
  esac

  if [ -f "artifact/$PLATFORM/$BINARY_NAME" ]; then
    echo "✅ Built artifact/$PLATFORM/$BINARY_NAME"
  else
    echo "❌ Failed to build for $PLATFORM"
  fi
done

echo ""
echo "=========================================="
echo "✅ Cross-build complete!"
echo "=========================================="
echo "📁 Files created:"
for PLATFORM in "${PLATFORMS[@]}"; do
  BINARY="artifact/$PLATFORM/vibe-kanban"
  [ "$PLATFORM" == "windows-x64" ] && BINARY="artifact/$PLATFORM/vibe-kanban.exe"
  if [ -f "$BINARY" ]; then
    SIZE=$(du -h "$BINARY" | cut -f1)
    echo "   - $BINARY ($SIZE)"
  fi
done
