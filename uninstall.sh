#!/bin/sh
# ells uninstaller for macOS / Linux
# Usage: curl -fsSL https://raw.githubusercontent.com/lg10/ells/main/uninstall.sh | sh
#        curl -fsSL https://raw.githubusercontent.com/lg10/ells/main/uninstall.sh | sh -s -- --purge
# Env vars:
#   ELLS_INSTALL_DIR  install dir to clean; defaults to ~/.local/bin
#   ELLS_PURGE        1 = also delete ~/.ells (same effect as --purge)
#
# ~/.ells holds the encrypted vault (every host address, account and password),
# known_hosts and settings. It is KEPT by default: uninstalling the binary must
# not silently destroy credentials. Delete it yourself (or pass --purge) only
# when you are sure you will not need those hosts again -- there is no recovery.
#
# Same macOS/bash-3.2 rules as install.sh: every variable is pre-bound, every
# expansion is braced, and no multibyte character sits next to an expansion.
set -eu

PURGE=""
INSTALL_DIR=""
BIN_PATH=""
SHORT_PATH=""
PATH_LINE=""
PS_PROFILE=""
PS_MARKER=""
TMP_FILE=""
RC=""

info() { printf '\033[36m[ells]\033[0m %s\n' "$1"; }
warn() { printf '\033[33m[ells]\033[0m %s\n' "$1"; }
fail() { printf '\033[31m[ells] 卸载失败: %s\n' "$1" >&2; exit 1; }

case "${1-}" in
  --purge) PURGE="1" ;;
  "")      PURGE="" ;;
  *)       fail "未知参数: ${1-} -- 只支持 --purge（同时删除 ~/.ells）" ;;
esac

if [ "${ELLS_PURGE+set}" = set ] && [ -n "${ELLS_PURGE-}" ]; then
  PURGE="$ELLS_PURGE"
fi

if [ "${ELLS_INSTALL_DIR+set}" = set ] && [ -n "$ELLS_INSTALL_DIR" ]; then
  INSTALL_DIR="$ELLS_INSTALL_DIR"
else
  INSTALL_DIR="$HOME/.local/bin"
fi

BIN_PATH="$INSTALL_DIR/ells"
SHORT_PATH="$INSTALL_DIR/s"

remove_bin() {
  # -e is false for a dangling symlink, so check -L too
  if [ -e "$1" ] || [ -L "$1" ]; then
    rm -f "$1" || fail "无法删除 $1"
    info "已删除 $1"
    return 0
  fi
  return 1
}

remove_bin "$BIN_PATH" || warn "没有找到 ${BIN_PATH} -- 可能已经卸载，或装在别处（用 ELLS_INSTALL_DIR 指定）"
remove_bin "$SHORT_PATH" || true

# install.sh 只往当前 SHELL 对应的 rc 里写 PATH，但用户可能换过 shell，
# 所以三个候选文件都清一遍；顺带把可能的 zsh/bash 配置一起弄干净是安全的。
PATH_LINE="export PATH=\"$INSTALL_DIR:\$PATH\""
for RC in "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.profile"; do
  [ -f "$RC" ] || continue
  grep -qF "$PATH_LINE" "$RC" || continue
  TMP_FILE="$RC.ells-uninstall-tmp"
  # 只删等于这一行的内容；并且只有确实少行才回写，绝不清空用户配置
  awk -v needle="$PATH_LINE" '$0 != needle' "$RC" > "$TMP_FILE" || fail "无法处理 $RC"
  if [ "$(wc -l < "$TMP_FILE")" -lt "$(wc -l < "$RC")" ]; then
    cat "$TMP_FILE" > "$RC"
    info "已从 $RC 移除 PATH 配置"
  fi
  rm -f "$TMP_FILE"
done

# 安装脚本在 PowerShell 配置里注册的 s 函数块（标记行到第一个单独的 }）
PS_PROFILE="$HOME/.config/powershell/Microsoft.PowerShell_profile.ps1"
PS_MARKER='# ells: short command s'
if [ -f "$PS_PROFILE" ] && grep -qF "$PS_MARKER" "$PS_PROFILE"; then
  TMP_FILE="$PS_PROFILE.ells-uninstall-tmp"
  awk -v marker="$PS_MARKER" '
    $0 == marker { skip = 1; next }
    skip { if ($0 == "}") { skip = 0 }; next }
    { print }
  ' "$PS_PROFILE" > "$TMP_FILE" || fail "无法处理 $PS_PROFILE"
  if [ "$(wc -l < "$TMP_FILE")" -lt "$(wc -l < "$PS_PROFILE")" ]; then
    cat "$TMP_FILE" > "$PS_PROFILE"
    info "已从 PowerShell 配置移除 s 函数"
  fi
  rm -f "$TMP_FILE"
fi

if [ "$PURGE" = "1" ] || [ "$PURGE" = "y" ] || [ "$PURGE" = "Y" ]; then
  if [ -d "$HOME/.ells" ]; then
    rm -rf "$HOME/.ells" || fail "无法删除 ~/.ells"
    warn "已删除 ${HOME}/.ells -- 保险库连同全部主机凭据无法恢复"
  fi
else
  warn "已保留 ${HOME}/.ells：里面有保险库、known_hosts 与设置"
  info "确认不再需要时执行 rm -rf ~/.ells（重装会自动新建）"
fi

info "卸载完成，重新打开终端后 PATH 里的残留也会消失"
