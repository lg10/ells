#!/bin/sh
# ells installer for macOS / Linux
# Usage: curl -fsSL https://raw.githubusercontent.com/lg10/ells/main/install.sh | sh
# Env vars:
#   ELLS_VERSION      pin a version (e.g. v0.1.1); defaults to the latest GitHub release
#   ELLS_INSTALL_DIR  install dir; defaults to ~/.local/bin
#   ELLS_API_URL      override the release-lookup endpoint (mirror / intranet)
#   ELLS_DOWNLOAD_URL override the release download base (mirror / intranet)
#
# Two macOS/bash-3.2 constraints that shaped this script:
#   1. /bin/sh on macOS is GNU bash 3.2. Under `set -u` expanding a variable that
#      was never assigned is fatal, so every variable is pre-bound and optionals are
#      probed with ${VAR+set} (which never expands the value itself).
#   2. bash 3.2 in a non-UTF-8 locale folds the multibyte bytes that follow an
#      unbraced expansion into the parameter name, so "$VERSION" + a fullwidth
#      parenthesis looks up VERSION<EF><BC><88> and dies with
#      "VERSION?: unbound variable". Therefore every expansion is braced AND every
#      character adjacent to an expansion is ASCII. Messages stay Chinese elsewhere.
set -eu

REPO="lg10/ells"
BIN_NAME="ells"
OS=""
ASSET=""
VERSION=""
BASE_URL=""
API_URL=""
INSTALL_DIR=""
TMP=""
SUM_TOOL=""
ACTUAL=""
EXPECTED=""
added_to=""
PATH_LINE=""

info() { printf '\033[36m[ells]\033[0m %s\n' "$1"; }
fail() { printf '\033[31m[ells] 安装失败: %s\n' "$1" >&2; exit 1; }

command -v curl >/dev/null 2>&1 || fail "需要 curl, 请先安装后重试"

case "$(uname -s)" in
  Linux)  OS="linux" ;;
  Darwin) OS="macos" ;;
  *)      fail "不支持的系统: $(uname -s) -- 仅支持 macOS / Linux, Windows 请用 install.ps1" ;;
esac

if [ "$OS" = "macos" ]; then
  ASSET="ells-macos-universal"
else
  case "$(uname -m)" in
    x86_64|amd64) ASSET="ells-linux-x86_64" ;;
    *)            fail "暂不支持的 Linux 架构: $(uname -m) -- 当前仅发布 x86_64" ;;
  esac
fi

if [ "${ELLS_INSTALL_DIR+set}" = set ] && [ -n "$ELLS_INSTALL_DIR" ]; then
  INSTALL_DIR="$ELLS_INSTALL_DIR"
else
  INSTALL_DIR="$HOME/.local/bin"
fi

if [ "${ELLS_API_URL+set}" = set ] && [ -n "$ELLS_API_URL" ]; then
  API_URL="$ELLS_API_URL"
else
  API_URL="https://api.github.com/repos/$REPO/releases/latest"
fi

if [ "${ELLS_VERSION+set}" = set ] && [ -n "$ELLS_VERSION" ]; then
  VERSION="$ELLS_VERSION"
else
  info "正在获取最新版本号..."
  VERSION=$(curl -fsSL "$API_URL" | grep '"tag_name"' | head -n1 | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/') || VERSION=""
  [ -n "$VERSION" ] || fail "无法获取最新版本号, 请设置 ELLS_VERSION=vX.Y.Z 后重试"
fi

if [ "${ELLS_DOWNLOAD_URL+set}" = set ] && [ -n "$ELLS_DOWNLOAD_URL" ]; then
  BASE_URL="$ELLS_DOWNLOAD_URL/$VERSION"
else
  BASE_URL="https://github.com/$REPO/releases/download/$VERSION"
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

info "正在下载 ${BIN_NAME} ${VERSION} -- ${ASSET}"
curl -fsSL -o "$TMP/$ASSET" "$BASE_URL/$ASSET"
curl -fsSL -o "$TMP/SHA256SUMS.txt" "$BASE_URL/SHA256SUMS.txt" \
  || fail "无法获取 SHA256SUMS.txt 校验文件, 版本可能过旧或发布不完整"

if command -v shasum >/dev/null 2>&1; then
  SUM_TOOL="shasum -a 256"
elif command -v sha256sum >/dev/null 2>&1; then
  SUM_TOOL="sha256sum"
else
  fail "系统中没有 shasum / sha256sum, 无法校验下载文件"
fi
ACTUAL=$($SUM_TOOL "$TMP/$ASSET" | awk '{print $1}')
EXPECTED=$(grep "$ASSET" "$TMP/SHA256SUMS.txt" | awk '{print $1}')
[ -n "$EXPECTED" ] || fail "SHA256SUMS.txt 中缺少 ${ASSET} 的校验条目"
[ "$ACTUAL" = "$EXPECTED" ] || fail "SHA256 校验不匹配, 下载文件可能被篡改"
info "SHA256 校验通过"

mkdir -p "$INSTALL_DIR"
cp "$TMP/$ASSET" "$INSTALL_DIR/$BIN_NAME"
chmod 755 "$INSTALL_DIR/$BIN_NAME"
ln -sf "$BIN_NAME" "$INSTALL_DIR/s"
info "已安装: ${INSTALL_DIR}/${BIN_NAME} 以及 ${INSTALL_DIR}/s"

# Make sure the install dir is on PATH (rustup style); write the actual dir so a
# custom ELLS_INSTALL_DIR keeps working.
PATH_LINE="export PATH=\"$INSTALL_DIR:\$PATH\""
case "${SHELL-x}" in
  */zsh)  added_to="$HOME/.zshrc" ;;
  */bash) added_to="$HOME/.bashrc" ;;
esac
[ -n "$added_to" ] || added_to="$HOME/.profile"
if ! printf '%s' ":$PATH:" | grep -qF ":$INSTALL_DIR:"; then
  mkdir -p "$(dirname "$added_to")"; touch "$added_to"
  grep -qF "$PATH_LINE" "$added_to" || printf '\n%s\n' "$PATH_LINE" >> "$added_to"
  info "已将安装目录写入 ${added_to}"
fi

# In PowerShell a function wins over a PATH executable and `s` is a built-in alias
# (Set-Variable), so register a function in the profile to make `s` work there too.
PS_PROFILE="$HOME/.config/powershell/Microsoft.PowerShell_profile.ps1"
PS_MARKER='# ells: short command s'
if ! grep -qF "$PS_MARKER" "$PS_PROFILE" 2>/dev/null; then
  mkdir -p "$(dirname "$PS_PROFILE")"; touch "$PS_PROFILE"
  printf '\n%s\nif (Test-Path "%s/s") {\n    function global:s { & "%s/s" $%s }\n}\n' "$PS_MARKER" "$INSTALL_DIR" "$INSTALL_DIR" 'args' >> "$PS_PROFILE"
  info "已为 PowerShell 注册 s 命令: ${PS_PROFILE}"
fi

info "安装完成 ${VERSION}, 重新打开终端后即可使用 ells 或 s"
