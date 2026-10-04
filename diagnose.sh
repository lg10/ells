#!/bin/sh
# ells 安装问题诊断（macOS / Linux）
# 用法：curl -fsSL https://raw.githubusercontent.com/lg10/ells/main/diagnose.sh | sh
set -e

DIAG_DIR=$(mktemp -d)
trap 'rm -rf "$DIAG_DIR"' EXIT

echo "== 系统 =="
echo "uname -s : $(uname -s)"
echo "uname -m : $(uname -m)"
echo "sw_vers  : $(sw_vers -productVersion 2>/dev/null || echo '(非 mac)')"

echo
echo "== shell =="
echo "SHELL 环境变量 : ${SHELL:-（未设置）}"
echo "sh 路径        : $(command -v sh)"
echo "sh 版本        : $(sh --version 2>/dev/null | head -n1 || echo '(无 --version，可能是 BSD sh/dash)')"
echo "当前脚本由谁跑 : \$0 = $0"

echo
echo "== 相关环境变量（若有残留会干扰安装脚本）=="
echo "ELLS_VERSION      = ${ELLS_VERSION:-（未设置）}"
echo "ELLS_INSTALL_DIR  = ${ELLS_INSTALL_DIR:-（未设置）}"
echo "ELLS_API_URL      = ${ELLS_API_URL:-（未设置）}"
echo "ELLS_DOWNLOAD_URL = ${ELLS_DOWNLOAD_URL:-（未设置）}"

echo
echo "== 网络 =="
code=$(curl -fsSL -o /dev/null -w '%{http_code}' https://api.github.com/repos/lg10/ells/releases/latest 2>&1) || code="失败"
echo "api.github.com/releases/latest -> $code"

echo
echo "== 正式跑一次安装（失败则自动给出 trace 最后 25 行）=="
curl -fsSL https://raw.githubusercontent.com/lg10/ells/main/install.sh -o "$DIAG_DIR/install.sh"
if LC_ALL=C sh "$DIAG_DIR/install.sh"; then
  echo ">>> 安装成功：ells --version 应输出当前最新版本的 ells"
else
  echo ">>> 普通安装失败，改用 sh -x 追踪模式重跑（只看最后 25 行）："
  LC_ALL=C sh -x "$DIAG_DIR/install.sh" 2>&1 | tail -n 25
fi
