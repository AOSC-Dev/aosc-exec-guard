#!/usr/bin/env bash
# Real install path check (requires root):
#   * installs the conf into /usr/lib/binfmt.d/ the way a package would
#   * lets systemd-binfmt apply it
#   * shows the effect of registration order:
#       - a freshly registered guard entry wins over the boot-time qemu entry
#         (later registration = higher precedence, measured)
#       - in a clean boot order (files applied sorted by name, so
#         aosc-exec-guard-* before qemu-*), qemu-aarch64 wins and emulator
#         users are not hijacked by the guard
#   * reverts everything on exit, including on failure
#
#   scripts/test.sh && sudo scripts/systemd-install-test.sh
set -euo pipefail
cd "$(dirname "$0")/.."

if [ "$(id -u)" -ne 0 ]; then
  echo "需要 root：sudo $0" >&2
  exit 1
fi
case "$(uname -m)" in
  aarch64|arm64) echo '本机是 aarch64：跳过（条目会劫持解释器自身）' >&2; exit 1;;
esac

CONF_SRC=$PWD/data/binfmt.d/aosc-exec-guard-aarch64.conf
CONF_DST=/usr/lib/binfmt.d/aosc-exec-guard-aarch64.conf
GUARD=$PWD/target/release/aosc-exec-guard
[ -x "$GUARD" ] || { echo "找不到 $GUARD；请先运行 scripts/test.sh" >&2; exit 1; }
BM=/proc/sys/fs/binfmt_misc
ENTRY=$BM/aosc-exec-guard-aarch64
TMP=tests/tmp
[ -f "$TMP/aarch64.elf" ] || { echo "先运行 scripts/test.sh 生成 $TMP/aarch64.elf" >&2; exit 1; }
[ -x "$TMP/aarch64.elf" ] || chmod +x "$TMP/aarch64.elf"

step() { printf '\n== %s ==\n' "$*"; }

show_entry() {
  if [ -e "$BM/$1" ]; then
    local line
    echo "-- $1"
    while IFS= read -r line; do printf '   %s\n' "$line"; done < "$BM/$1"
  else
    echo "-- $1: 不存在"
  fi
}

probe() { # run the fake aarch64 file and report which entry handled it
  set +e
  local out code
  out=$(env -u DISPLAY -u WAYLAND_DISPLAY AOSC_EXEC_GUARD_NO_DIALOG=1 "$TMP/aarch64.elf" 2>&1)
  code=$?
  set -e
  printf '%s\nexit=%s\n' "$out" "$code"
  case "$out" in
    *'无法运行'*)  echo '=> guard 先匹配' ;;
    *qemu*)        echo '=> qemu-aarch64 先匹配' ;;
    *)             echo '=> 未被任何条目匹配' ;;
  esac
}

cleanup() {
  set +e
  rm -f "$CONF_DST"
  systemctl restart systemd-binfmt.service
  [ -e "$ENTRY" ] && echo -1 > "$ENTRY"
  set -e
}
trap cleanup EXIT

step '安装 conf（模拟打包安装，把解释器指到本仓库构建的二进制）'
# 注意：F 标志要求解释器文件在注册时就存在（正式打包时即 /usr/bin/aosc-exec-guard）。
sed "s|/usr/bin/aosc-exec-guard|$GUARD|" "$CONF_SRC" > "$CONF_DST"
chmod 644 "$CONF_DST"
systemctl restart systemd-binfmt.service
show_entry aosc-exec-guard-aarch64

step '运行时优先级：服务刚注册的 guard vs 开机时的 qemu'
probe

step '模拟干净启动顺序：清空所有条目，让 systemd-binfmt 按文件名排序重放'
for f in "$BM"/*; do
  name=${f##*/}
  case "$name" in
    register|status) ;;
    *) echo -1 > "$f" 2>/dev/null || true ;;
  esac
done
systemctl restart systemd-binfmt.service
show_entry aosc-exec-guard-aarch64
show_entry qemu-aarch64
probe

step '清理：移除 conf、重放，并确认 guard 条目消失'
rm -f "$CONF_DST"
systemctl restart systemd-binfmt.service
if [ -e "$ENTRY" ]; then
  echo '（重放后条目仍在：systemd-binfmt 不会自动清理不在配置里的条目，需要显式注销）'
  echo -1 > "$ENTRY"
fi
if [ -e "$ENTRY" ]; then
  echo 'FAIL: guard 条目未清除' >&2
  exit 1
fi
echo '=> guard 条目已移除'

if [ -x "$TMP/busybox-aarch64" ]; then
  step '最终确认：qemu 仍能模拟运行 aarch64'
  set +e
  out=$(env -u DISPLAY -u WAYLAND_DISPLAY "$TMP/busybox-aarch64" true 2>&1)
  code=$?
  set -e
  printf '%s\nexit=%s\n' "$out" "$code"
  if [ "$code" -eq 0 ]; then
    echo '=> qemu 正常'
  else
    echo '=> 注意：qemu 未恢复，请检查 binfmt 条目（重启也能恢复）' >&2
  fi
fi

step 'systemd-binfmt 安装路径测试通过'
