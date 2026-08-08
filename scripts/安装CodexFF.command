#!/bin/bash
# CodexFF 双击安装助手
#
# 用法: 双击本文件 → 终端自动打开 → 把 CodexFF.app 拖进窗口 → 回车
# 作用: 安装到 /Applications + 清除隔离属性 + 校验签名 + 启动

set -e

echo "=============================================="
echo " CodexFF 安装助手"
echo "=============================================="
echo ""
echo "请把 CodexFF.app 拖进这个窗口, 然后按回车:"
read -r -p "路径: " RAW

# 拖拽路径可能带引号或转义空格, 做基本清洗
APP="${RAW%\"}"
APP="${APP#\"}"
APP="${APP//\\ / }"

if [ ! -d "$APP" ] || [ "$(basename "$APP")" != "CodexFF.app" ]; then
  echo ""
  echo "❌ 没有识别到 CodexFF.app, 请重新拖入再回车。"
  echo ""
  read -r -p "按回车关闭窗口…"
  exit 1
fi

echo ""
echo "正在退出旧版 CodexFF (如果正在运行)…"
osascript -e 'quit app "CodexFF"' 2>/dev/null || true
sleep 1

echo "正在安装到 /Applications …"
rm -rf "/Applications/CodexFF.app"
ditto "$APP" "/Applications/CodexFF.app"

echo "正在清除隔离属性 (Gatekeeper)…"
xattr -dr com.apple.quarantine "/Applications/CodexFF.app" 2>/dev/null || true

echo "正在校验签名…"
if codesign --verify --strict "/Applications/CodexFF.app"; then
  echo "✅ 签名校验通过"
else
  echo "⚠️ 签名校验未通过 (不影响本次运行, 但请重新下载安装包)"
fi

echo "正在启动 CodexFF…"
open "/Applications/CodexFF.app"
echo ""
echo "✅ 安装完成: /Applications/CodexFF.app"
echo ""
read -r -p "按回车关闭窗口…"
