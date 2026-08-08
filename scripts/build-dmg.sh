#!/bin/bash
# 打包正式发布 DMG: CodexFF.app + 安装CodexFF.command + 安装说明.txt
set -e
cd "$(dirname "$0")/.."

APP=src-tauri/target/release/bundle/macos/CodexFF.app
if [ ! -d "$APP" ]; then
  echo "❌ 未找到 $APP，请先执行 npm run build:app"
  exit 1
fi

VER=$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" "$APP/Contents/Info.plist" 2>/dev/null || cat version.txt)
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT

mkdir -p "$STAGE/CodexFF" dist
cp -R "$APP" "$STAGE/CodexFF/CodexFF.app"
cp "scripts/安装CodexFF.command" "$STAGE/CodexFF/"
cp "scripts/安装说明.txt" "$STAGE/CodexFF/"
# 应用程序快捷方式: 用户直接把 App 拖到它上面即可完成安装
ln -s /Applications "$STAGE/CodexFF/应用程序"
# 给安装脚本 ad-hoc 签名, 降低 Gatekeeper 对 .command 的拦截概率
codesign --force --sign - "$STAGE/CodexFF/安装CodexFF.command" 2>/dev/null || true

hdiutil create -volname CodexFF -srcfolder "$STAGE/CodexFF" -ov -format UDZO "dist/CodexFF-${VER}.dmg"
echo "✅ 已生成: dist/CodexFF-${VER}.dmg"
