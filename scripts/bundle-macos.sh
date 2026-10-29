#!/bin/bash
# 把 release 二进制打成 macOS .app（不依赖 tauri-cli 版本）。
# 本机可能只装了 tauri-cli v1，读不了 v2 的 tauri.conf.json，所以这里手工组装 bundle。
#
# 用法: ./scripts/bundle-macos.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/gui/src-tauri/target/release/dmde-gui"
ICON="$ROOT/gui/src-tauri/icons/icon.png"
APP="$ROOT/dist/dmde-rs.app"

if [ ! -x "$BIN" ]; then
  echo "先构建 release：cd gui/src-tauri && cargo build --release" >&2
  exit 1
fi

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/dmde-rs"

# png → iconset → icns
if [ -f "$ICON" ] && command -v iconutil >/dev/null; then
  SET="$(mktemp -d)/icon.iconset"
  mkdir -p "$SET"
  for s in 16 32 64 128 256 512; do
    sips -z $s $s "$ICON" --out "$SET/icon_${s}x${s}.png" >/dev/null
    d=$((s * 2))
    sips -z $d $d "$ICON" --out "$SET/icon_${s}x${s}@2x.png" >/dev/null
  done
  iconutil -c icns "$SET" -o "$APP/Contents/Resources/icon.icns"
fi

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>dmde-rs</string>
  <key>CFBundleDisplayName</key><string>dmde-rs</string>
  <key>CFBundleIdentifier</key><string>cn.chenddcoder.dmde-rs</string>
  <key>CFBundleVersion</key><string>0.1.0</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleExecutable</key><string>dmde-rs</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleIconFile</key><string>icon.icns</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

printf 'APPL????' > "$APP/Contents/PkgInfo"

# ad-hoc 签名，避免 Gatekeeper 直接拦（本地运行够用）
codesign --force --deep --sign - "$APP" >/dev/null 2>&1 || echo "（ad-hoc 签名失败，可忽略）"

echo "已生成: $APP"
echo "打开:   open \"$APP\""
