#!/bin/sh
# ells installer for macOS / Linux
# Usage: curl -fsSL https://raw.githubusercontent.com/lg10/ells/main/install.sh | sh
# Env vars:
#   ELLS_VERSION         pin a version (e.g. v0.1.1); defaults to the latest GitHub release
#   ELLS_INSTALL_DIR     install dir; defaults to ~/.local/bin
#   ELLS_UPDATE_SOURCES  comma/semicolon/space separated sources: `github` or a
#                        mirror prefix (e.g. https://gh-proxy.com). Empty = built-ins.
#   ELLS_API_URL / ELLS_DOWNLOAD_URL are deprecated and ignored.
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
INSTALL_DIR=""
TMP=""
SUM_TOOL=""
ACTUAL=""
EXPECTED=""
STRICT=""
SRC_SUM=""
INSTALLED=""
added_to=""
PATH_LINE=""
SOURCES=""
RAW_SOURCES=""

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

if [ "${ELLS_API_URL+set}" = set ] || [ "${ELLS_DOWNLOAD_URL+set}" = set ]; then
  info "ELLS_API_URL / ELLS_DOWNLOAD_URL 已废弃并被忽略; 请改用 ELLS_UPDATE_SOURCES"
fi

if [ "${ELLS_UPDATE_SOURCES+set}" = set ] && [ -n "$ELLS_UPDATE_SOURCES" ]; then
  RAW_SOURCES=$(printf '%s' "$ELLS_UPDATE_SOURCES" | tr ',;' '  ')
else
  RAW_SOURCES="github https://gh-proxy.com https://ghproxy.net https://ghfast.top https://gh.llkk.cc"
fi

for src in $RAW_SOURCES; do
  case "$src" in
    github|http://*|https://*) ;;
    *) info "忽略无效的更新源: ${src}"; continue ;;
  esac
  case " ${SOURCES} " in
    *" ${src} "*) ;;
    *) SOURCES="${SOURCES} ${src}" ;;
  esac
done
SOURCES=${SOURCES# }
[ -n "$SOURCES" ] || fail "没有可用的更新源, 请检查 ELLS_UPDATE_SOURCES"

# 官方直连返回原 URL; 镜像前缀把完整 GitHub URL 拼在后面.
source_url() {
  if [ "$1" = "github" ]; then
    printf '%s' "$2"
  else
    printf '%s/%s' "${1%/}" "$2"
  fi
}

source_label() {
  if [ "$1" = "github" ]; then
    printf 'github'
  else
    _label=${1#https://}
    _label=${_label#http://}
    printf '%s' "${_label%%/*}"
  fi
}

if [ "${ELLS_VERSION+set}" = set ] && [ -n "$ELLS_VERSION" ]; then
  VERSION="$ELLS_VERSION"
else
  info "正在获取最新版本号..."
  for src in $SOURCES; do
    api=$(source_url "$src" "https://api.github.com/repos/${REPO}/releases/latest")
    body=$(curl -fsSL --connect-timeout 3 --max-time 8 "$api" 2>/dev/null) || body=""
    candidate=$(printf '%s' "$body" | grep '"tag_name"' | head -n1 | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')
    if [ -n "$candidate" ]; then
      VERSION="$candidate"
      info "版本号来自 $(source_label "$src")"
      break
    fi
  done
  [ -n "$VERSION" ] || fail "无法获取最新版本号, 请设置 ELLS_VERSION=vX.Y.Z 后重试"
fi

case "$VERSION" in
  v[0-9]*) ;;
  *) fail "版本号格式不对: ${VERSION}" ;;
esac

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

if command -v shasum >/dev/null 2>&1; then
  SUM_TOOL="shasum -a 256"
elif command -v sha256sum >/dev/null 2>&1; then
  SUM_TOOL="sha256sum"
else
  fail "系统中没有 shasum / sha256sum, 无法校验下载文件"
fi

# 先把各源的校验文件都取一遍. 官方直连优先; 否则至少两个源一致才算交叉核对通过.
info "正在核对更新源的校验文件..."
: > "$TMP/all-sums"
: > "$TMP/github-sum"
i=0
for src in $SOURCES; do
  i=$((i+1))
  sums_url=$(source_url "$src" "https://github.com/${REPO}/releases/download/${VERSION}/SHA256SUMS.txt")
  if ! curl -fsSL --connect-timeout 3 --max-time 10 -o "$TMP/sums.$i" "$sums_url" 2>/dev/null; then
    info "源 $(source_label "$src") 取不到校验文件, 跳过"
    continue
  fi
  EXPECTED=$(grep -F "$ASSET" "$TMP/sums.$i" | awk '{print $1}' | head -n1 | tr 'A-Z' 'a-z')
  if [ -z "$EXPECTED" ]; then
    info "源 $(source_label "$src") 的校验文件里没有 ${ASSET}"
    continue
  fi
  printf '%s\n' "$EXPECTED" >> "$TMP/all-sums"
  printf '%s\n' "$EXPECTED" > "$TMP/sum.$i"
  printf '%s\n' "$src" > "$TMP/src.$i"
  if [ "$src" = "github" ]; then
    printf '%s\n' "$EXPECTED" > "$TMP/github-sum"
  fi
done

if [ -s "$TMP/github-sum" ]; then
  STRICT=$(head -n1 "$TMP/github-sum")
elif [ -s "$TMP/all-sums" ]; then
  STRICT=$(sort "$TMP/all-sums" | uniq -d | head -n1)
fi
if [ -n "$STRICT" ]; then
  :
elif [ -s "$TMP/all-sums" ]; then
  if [ "$(wc -l < "$TMP/all-sums" | tr -d ' ')" -gt 1 ]; then
    info "各源的校验和不一致, 无法交叉核对; 将按源逐个校验"
  else
    info "只有一个源提供校验和, 无法交叉核对; 将按源逐个校验"
  fi
else
  fail "所有更新源都取不到校验文件, 请稍后重试或检查网络"
fi

# 下载顺序: 先试与可信校验和一致的源, 再试其余源 (仅在没有可信共识时才放宽)
ORDERED=""
i=0
for src in $SOURCES; do
  i=$((i+1))
  [ -f "$TMP/sum.$i" ] || continue
  CHECK=$(cat "$TMP/sum.$i")
  if [ -n "$STRICT" ] && [ "$CHECK" != "$STRICT" ]; then
    continue
  fi
  ORDERED="${ORDERED} ${src}"
done
ORDERED=${ORDERED# }
[ -n "$ORDERED" ] || fail "没有更新源提供与校验和一致的下载"

# 找到某个源自己的校验和 (没有就跳过)
find_sum() {
  _i=0
  for _src in $SOURCES; do
    _i=$((_i+1))
    if [ "$_src" = "$1" ] && [ -f "$TMP/sum.$_i" ]; then
      cat "$TMP/sum.$_i"
      return
    fi
  done
}

for src in $ORDERED; do
  SRC_SUM=$(find_sum "$src")
  [ -n "$SRC_SUM" ] || continue
  if [ -n "$STRICT" ] && [ "$SRC_SUM" != "$STRICT" ]; then
    continue
  fi
  attempt=1
  while [ "$attempt" -le 2 ]; do
    info "正在从 $(source_label "$src") 下载 ${BIN_NAME} ${VERSION} -- ${ASSET} (第 ${attempt}/2 次)"
    rm -f "$TMP/$ASSET"
    asset_url=$(source_url "$src" "https://github.com/${REPO}/releases/download/${VERSION}/${ASSET}")
    if curl -fsSL --connect-timeout 5 --max-time 300 -o "$TMP/$ASSET" "$asset_url" 2>/dev/null; then
      ACTUAL=$($SUM_TOOL "$TMP/$ASSET" | awk '{print $1}' | tr 'A-Z' 'a-z')
      if [ "$ACTUAL" = "$SRC_SUM" ]; then
        size=$(wc -c < "$TMP/$ASSET" | tr -d ' ')
        magic=$(od -An -N4 -tx1 "$TMP/$ASSET" | tr -d ' \n')
        if [ "$size" -lt 65536 ]; then
          info "源 $(source_label "$src") 返回的内容太短, 换源"
        else
          case "$OS:$magic" in
            linux:7f454c46) INSTALLED=1 ;;
            macos:feedface|macos:cefaedfe|macos:feedfacf|macos:cffaedfe|macos:cafebabe) INSTALLED=1 ;;
            *) info "源 $(source_label "$src") 返回的不是本平台二进制, 换源" ;;
          esac
        fi
        if [ -n "$INSTALLED" ]; then
          info "SHA256 校验通过"
          break 2
        fi
      else
        info "源 $(source_label "$src") 的 SHA256 校验不通过, 换源或重试"
      fi
    else
      info "源 $(source_label "$src") 下载失败, 换源或重试"
    fi
    attempt=$((attempt+1))
    sleep 1
  done
done

[ -n "$INSTALLED" ] || fail "所有更新源都没能提供完整且校验通过的文件, 请稍后重试"

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
