#!/usr/bin/env bash
# Kernel-level end-to-end test (requires root):
#   * registers the guard as a binfmt_misc interpreter
#   * observes which entry wins when both the guard and qemu-aarch64 match
#   * runs a fabricated AArch64 ELF (and, if downloaded, a real one) through
#     the kernel and checks the explanation + exit status 126
#   * checks that AOSC_EXEC_GUARD_QEMU=always hands the program over to qemu
#   * restores everything on exit, including on failure
#
#   scripts/test.sh && sudo scripts/kernel-test.sh
set -euo pipefail
cd "$(dirname "$0")/.."

if [ "$(id -u)" -ne 0 ]; then
  echo "需要 root：sudo $0" >&2
  exit 1
fi

GUARD=$PWD/target/release/aosc-exec-guard
[ -x "$GUARD" ] || { echo "找不到 $GUARD；请先运行 scripts/test.sh" >&2; exit 1; }

TMP=tests/tmp
mkdir -p "$TMP/home"
[ -f "$TMP/aarch64.elf" ] || { echo "找不到测试文件 $TMP/aarch64.elf；请先运行 scripts/test.sh" >&2; exit 1; }
[ -x "$TMP/aarch64.elf" ] || chmod +x "$TMP/aarch64.elf"

BM=/proc/sys/fs/binfmt_misc
ENTRY=$BM/aosc-exec-guard-aarch64
QEMU_ENTRY=$BM/qemu-aarch64

step() { printf '\n== %s ==\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

# Read a (procfs) file with shell builtins only: works even when exec is broken.
show_file() { local line; while IFS= read -r line; do printf '  %s\n' "$line"; done < "$1"; }

# Run a foreign binary the way a user would: text mode so that no dialog ever
# blocks this script, a private HOME so no saved “不再询问” answer leaks in, and
# AOSC_EXEC_GUARD_QEMU=never so the guard explains instead of asking/running.
run_target() {
  env -u DISPLAY -u WAYLAND_DISPLAY -u XDG_CONFIG_HOME AOSC_EXEC_GUARD_NO_DIALOG=1 \
    AOSC_EXEC_GUARD_QEMU=never HOME="$PWD/$TMP/home" "$@"
}

QEMU_WAS_ENABLED=no
if [ -e "$QEMU_ENTRY" ]; then
  qemu_first_line=''
  read -r qemu_first_line < "$QEMU_ENTRY" || true
  [ "$qemu_first_line" = enabled ] && QEMU_WAS_ENABLED=yes
fi

cleanup() {
  set +e
  [ -e "$ENTRY" ] && echo -1 > "$ENTRY"
  if [ -e "$QEMU_ENTRY" ]; then
    if [ "$QEMU_WAS_ENABLED" = yes ]; then echo 1 > "$QEMU_ENTRY"; else echo 0 > "$QEMU_ENTRY"; fi
  fi
  set -e
}
trap cleanup EXIT

# An entry matching the *host* architecture would hijack the interpreter
# itself and send the kernel into an exec recursion loop (see README). This
# test registers an AArch64 entry, so refuse to run on an AArch64 host.
case "$(uname -m)" in
  aarch64|arm64) fail '本机就是 aarch64：这个实验不能在它上面做';;
esac

step 'binfmt_misc 状态'
[ -e "$BM/status" ] || mount -t binfmt_misc binfmt_misc "$BM"
show_file "$BM/status"

step "注册 guard 条目（interpreter=$GUARD）"
[ -e "$ENTRY" ] && echo -1 > "$ENTRY"
line=$(grep -F ':aosc-exec-guard-aarch64:' data/binfmt.d/zz-aosc-exec-guard.conf)
line=${line//\/usr\/bin\/aosc-exec-guard/$GUARD}
printf '%s\n' "$line" > "$BM/register"
show_file "$ENTRY"

# Smoke test: the host must still be able to run its own binaries. If this
# breaks, the entry is wrong — abort and let the trap clean up.
if ! /usr/bin/true 2>/dev/null; then
  fail '注册后本机程序无法执行，已中止'
fi
echo '  （本机程序执行正常）'

step '优先级实验：guard 与 qemu-aarch64 同时可用时，谁先匹配'
set +e
out=$(run_target "$TMP/aarch64.elf" 2>&1)
code=$?
set -e
printf '%s\nexit=%s\n' "$out" "$code"
case "$out" in
  *'无法运行'*)   echo '=> guard 先匹配' ;;
  *qemu*)         echo '=> qemu-aarch64 先匹配' ;;
  *'Exec format error'*) echo '=> 未被任何条目匹配（注册失败？）' ;;
  *)              echo '=> 无法判断，原样输出见上' ;;
esac

step '暂时禁用 qemu-aarch64，验证 guard 的完整链路'
[ -e "$QEMU_ENTRY" ] && echo 0 > "$QEMU_ENTRY"
set +e
out=$(run_target "$TMP/aarch64.elf" 2>&1)
code=$?
set -e
printf '%s\nexit=%s\n' "$out" "$code"
[ "$code" -eq 126 ] || fail "exit code should be 126, got $code"
case "$out" in *aarch64*) ;; *) fail 'message should mention aarch64' ;; esac

if [ -x "$TMP/busybox-aarch64" ]; then
  step '真实 aarch64 二进制（busybox）走 guard'
  set +e
  out=$(run_target "$TMP/busybox-aarch64" 2>&1)
  code=$?
  set -e
  printf '%s\nexit=%s\n' "$out" "$code"
  [ "$code" -eq 126 ] || fail "exit code should be 126, got $code"
fi

if [ -x "$TMP/busybox-aarch64" ] && [ "$QEMU_WAS_ENABLED" = yes ]; then
  step 'guard 转发给 qemu：AOSC_EXEC_GUARD_QEMU=always 时 busybox 真的跑起来'
  # 转发要求 qemu 条目处于启用状态（上一步把它禁用过）
  [ -e "$QEMU_ENTRY" ] && echo 1 > "$QEMU_ENTRY"
  set +e
  out=$(env -u DISPLAY -u WAYLAND_DISPLAY -u XDG_CONFIG_HOME AOSC_EXEC_GUARD_QEMU=always \
    HOME="$PWD/$TMP/home" "$TMP/busybox-aarch64" uname -m 2>&1)
  code=$?
  set -e
  printf '%s\nexit=%s\n' "$out" "$code"
  [ "$code" -eq 0 ] || fail "busybox 应经 guard 交给 qemu 运行，实际退出码 $code"
  case "$out" in *aarch64*) ;; *) fail 'busybox 的 uname -m 应输出 aarch64' ;; esac
fi

step '恢复 qemu-aarch64，并确认模拟器行为恢复'
if [ "$QEMU_WAS_ENABLED" = yes ]; then echo 1 > "$QEMU_ENTRY"; fi
[ -e "$ENTRY" ] && echo -1 > "$ENTRY"
if [ -x "$TMP/busybox-aarch64" ] && [ "$QEMU_WAS_ENABLED" = yes ]; then
  set +e
  out=$(run_target "$TMP/busybox-aarch64" true 2>&1)
  code=$?
  set -e
  printf '%s\nexit=%s\n' "$out" "$code"
  if [ "$code" -eq 0 ]; then
    echo '=> qemu 模拟运行恢复正常'
  else
    fail 'qemu 未恢复，请检查 binfmt 条目'
  fi
fi

step '内核端到端测试通过'
