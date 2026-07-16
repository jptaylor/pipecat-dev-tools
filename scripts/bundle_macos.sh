#!/usr/bin/env bash
# Build a macOS .app bundle with an ad-hoc signature.
#
#   scripts/bundle_macos.sh              # current architecture
#   scripts/bundle_macos.sh --universal  # arm64 + x86_64 (needs both targets:
#                                        #   rustup target add x86_64-apple-darwin aarch64-apple-darwin)
#
# Output: dist/Pipecat Audio Metrics.app  (+ dist/pipecat-audio-metrics-macos.zip)

set -euo pipefail
cd "$(dirname "$0")/.."

APP_NAME="Pipecat Audio Metrics"
BIN_NAME="pipecat-audio-metrics"
DIST="dist"
export MACOSX_DEPLOYMENT_TARGET=12.0

if [[ "${1:-}" == "--universal" ]]; then
    echo "Building universal binary (arm64 + x86_64)..."
    cargo build --release --target aarch64-apple-darwin
    cargo build --release --target x86_64-apple-darwin
    mkdir -p target/release
    lipo -create \
        "target/aarch64-apple-darwin/release/$BIN_NAME" \
        "target/x86_64-apple-darwin/release/$BIN_NAME" \
        -output "target/release/$BIN_NAME"
else
    echo "Building release binary..."
    cargo build --release
fi

APP_DIR="$DIST/$APP_NAME.app"
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"

cp "target/release/$BIN_NAME" "$APP_DIR/Contents/MacOS/$BIN_NAME"
cp macos/Info.plist "$APP_DIR/Contents/Info.plist"

echo "Ad-hoc signing..."
codesign --force --deep --sign - "$APP_DIR"

( cd "$DIST" && rm -f "$BIN_NAME-macos.zip" && zip -qry "$BIN_NAME-macos.zip" "$APP_NAME.app" )

echo
echo "Done:"
echo "  $APP_DIR"
echo "  $DIST/$BIN_NAME-macos.zip"
echo
echo "First launch on another machine: right-click the app > Open (unsigned build),"
echo "or: xattr -d com.apple.quarantine '$APP_NAME.app'"
