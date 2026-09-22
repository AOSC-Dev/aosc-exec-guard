#!/usr/bin/env bash
# Local checks for the aosc-exec-guard PoC (no root, no real dialog windows):
#   * unit tests
#   * direct invocation against fabricated ELF files (foreign arch / native / non-ELF)
#   * dialog branch and qemu offer exercised through stub zenity / stub qemu
#
# The kernel-level end-to-end test is separate: sudo scripts/kernel-test.sh
set -euo pipefail
cd "$(dirname "$0")/.."

TMP=tests/tmp
mkdir -p "$TMP" "$TMP/home" "$TMP/binfmt-empty"

step() { printf '\n== %s ==\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

step 'cargo test'
cargo test --quiet

step 'cargo build --release'
cargo build --release --quiet
GUARD=$PWD/target/release/aosc-exec-guard

step 'fabricate test files'
# Minimal AArch64 ELF header (e_type=ET_EXEC, e_machine=0xb7): enough for
# binfmt_misc to match it in the kernel test, and for the guard to parse it.
{ printf '\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\xb7\x00\x01\x00\x00\x00'; head -c 236 /dev/zero; } > "$TMP/aarch64.elf"
# Same, but claiming x86_64: "native architecture, yet rejected".
{ printf '\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x3e\x00\x01\x00\x00\x00'; head -c 236 /dev/zero; } > "$TMP/x86_64.elf"
printf 'just some text\n' > "$TMP/not-an-elf"
chmod +x "$TMP/aarch64.elf" "$TMP/x86_64.elf"   # the kernel test execs these

# Environment isolation for direct guard runs: no GUI/service context, a
# private HOME (so a saved “不再询问” answer cannot leak in) and an explicit
# binfmt dir (so a host-installed qemu entry cannot change the outcome).
# stdin is /dev/null, so the terminal prompt never blocks a scripted run.
guard_env() { # $1 = binfmt dir, rest = command
  local binfmt_dir=$1
  shift
  env -u DISPLAY -u WAYLAND_DISPLAY -u XDG_SESSION_TYPE -u INVOCATION_ID -u XDG_CONFIG_HOME \
    HOME="$PWD/$TMP/home" AOSC_EXEC_GUARD_BINFMT_DIR="$binfmt_dir" "$@" </dev/null
}
run_guard() { guard_env "$PWD/$TMP/binfmt-empty" "$GUARD" "$@"; }
run_guard_qemu() { guard_env "$PWD/$TMP/binfmt" "$GUARD" "$@"; }

step 'direct: foreign arch (aarch64) → explain, exit 126'
set +e
out=$(AOSC_EXEC_GUARD_DEBUG=1 run_guard "$TMP/aarch64.elf" 2>&1)
code=$?
set -e
printf '%s\nexit=%s\n' "$out" "$code"
[ "$code" -eq 126 ] || fail "exit code should be 126, got $code"
case "$out" in *aarch64*) ;; *) fail 'message should mention aarch64' ;; esac
case "$out" in *x86_64*) ;; *) fail 'message should mention the host arch' ;; esac
case "$out" in *'mode=Text'*) ;; *) fail 'streams are piped and no display is set, so mode should be Text' ;; esac

step 'direct: native-but-rejected → explain, exit 126'
set +e
out=$(run_guard "$TMP/x86_64.elf" 2>&1)
code=$?
set -e
printf '%s\nexit=%s\n' "$out" "$code"
[ "$code" -eq 126 ] || fail "exit code should be 126, got $code"
case "$out" in *损坏*) ;; *) fail 'message should mention corruption' ;; esac

step 'direct: not an ELF → explain, exit 126'
set +e
out=$(run_guard "$TMP/not-an-elf" 2>&1)
code=$?
set -e
printf '%s\nexit=%s\n' "$out" "$code"
[ "$code" -eq 126 ] || fail "exit code should be 126, got $code"
case "$out" in *ELF*) ;; *) fail 'message should mention ELF' ;; esac

step 'CLI: --help / --version / missing target'
"$GUARD" --help >/dev/null || fail '--help should exit 0'
"$GUARD" --version >/dev/null || fail '--version should exit 0'
set +e
"$GUARD" >/dev/null 2>&1
code=$?
set -e
[ "$code" -eq 2 ] || fail "missing target should exit 2 (usage error), got $code"

step 'CLI: --debug is recognised before the program path'
set +e
out=$(run_guard --debug "$TMP/not-an-elf" 2>&1)
code=$?
set -e
[ "$code" -eq 126 ] || fail "exit code should be 126, got $code"
case "$out" in *'[debug] mode='*) ;; *) fail '--debug before the path should print the decision' ;; esac

step 'CLI: kernel-style arguments stay pass-through'
# binfmt_misc appends the original program's arguments; they must never be
# parsed as guard options.
set +e
out=$(run_guard "$TMP/not-an-elf" --debug --help -l 你好 2>&1)
code=$?
set -e
[ "$code" -eq 126 ] || fail "pass-through arguments should still exit 126, got $code"
case "$out" in *'[debug]'*) fail '--debug after the path must not reach the guard' ;; esac
case "$out" in *'Usage:'*) fail '--help after the path must not show the guard help' ;; esac

step 'CLI: non-UTF-8 argument does not break the guard'
set +e
out=$(run_guard "$TMP/not-an-elf" "$(printf '\xff')" 2>&1)
code=$?
set -e
[ "$code" -eq 126 ] || fail "non-UTF-8 argument should still exit 126, got $code"
case "$out" in *ELF*) ;; *) fail 'explanation should still be printed' ;; esac

step 'installer: 按家族过滤规则（可交叉打包）'
# x86_64 目标：整个 i386 家族（i386 + x86_64）都不该留下
scripts/install.sh --prefix "$TMP/pkg-x86_64" --host-arch x86_64 > "$TMP/install.log" 2>&1 \
  || fail "installer 在 x86_64 目标上失败：$(cat "$TMP/install.log")"
conf=$TMP/pkg-x86_64/lib/binfmt.d/zz-aosc-exec-guard.conf
[ -x "$TMP/pkg-x86_64/bin/aosc-exec-guard" ] || fail 'installer 没装二进制'
case "$(cat "$conf")" in
  *':aosc-exec-guard-i386:'*|*':aosc-exec-guard-x86_64:'*)
    fail 'x86_64 目标的 conf 里不该有 i386/x86_64 规则'
    ;;
esac
case "$(cat "$conf")" in
  *':aosc-exec-guard-aarch64:'*) ;;
  *) fail 'x86_64 目标的 conf 里应保留 aarch64 规则' ;;
esac
# aarch64 目标：arm 家族（arm + aarch64）都不该留下，x86_64 要留着
scripts/install.sh --prefix "$TMP/pkg-aarch64" --host-arch aarch64 > /dev/null 2>&1 \
  || fail 'installer 在 aarch64 目标上失败'
conf=$TMP/pkg-aarch64/lib/binfmt.d/zz-aosc-exec-guard.conf
case "$(cat "$conf")" in
  *':aosc-exec-guard-arm:'*|*':aosc-exec-guard-aarch64:'*)
    fail 'aarch64 目标的 conf 里不该有 arm/aarch64 规则'
    ;;
esac
case "$(cat "$conf")" in
  *':aosc-exec-guard-x86_64:'*) ;;
  *) fail 'aarch64 目标的 conf 里应保留 x86_64 规则' ;;
esac
# 默认（本机架构）：自检和过滤都不应该报错
scripts/install.sh --prefix "$TMP/pkg" > /dev/null || fail 'installer 在本机架构上失败'
conf=$TMP/pkg/lib/binfmt.d/zz-aosc-exec-guard.conf
echo "本机 $(uname -m)：过滤后剩 $(grep -c '^:' "$conf") 条规则"
# 卸载
scripts/install.sh --prefix "$TMP/pkg" --uninstall > /dev/null || fail 'uninstall 失败'
[ ! -e "$conf" ] || fail 'uninstall 没删 conf'
[ ! -e "$TMP/pkg/bin/aosc-exec-guard" ] || fail 'uninstall 没删二进制'

step 'stubs: fake binfmt dir + stub qemu + stub zenity'
mkdir -p "$TMP/bin" "$TMP/binfmt"
cat > "$TMP/bin/qemu-aarch64" <<'STUB'
#!/usr/bin/env bash
printf 'stub-qemu %s\n' "$*"
exit 42
STUB
chmod +x "$TMP/bin/qemu-aarch64"
cat > "$TMP/binfmt/qemu-aarch64" <<EOF
enabled
interpreter $PWD/$TMP/bin/qemu-aarch64
flags: OCF
EOF
# Stub zenity: logs its arguments to $AOSC_GUARD_TEST_LOG, and can pretend the
# user ticked “不再询问” or cancelled the dialog.
cat > "$TMP/bin/zenity" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$@" >> "$AOSC_GUARD_TEST_LOG"
[ -n "${AOSC_GUARD_TEST_ZENITY_CANCEL:-}" ] && exit 1
[ -n "${AOSC_GUARD_TEST_ZENITY_CHECKED:-}" ] && printf '不再询问\n'
exit 0
STUB
chmod +x "$TMP/bin/zenity"
LOG=$TMP/zenity.log
: > "$LOG"

step 'dialog branch (stub zenity, no window is opened)'
set +e
env -u INVOCATION_ID -u XDG_CONFIG_HOME DISPLAY=:99 AOSC_GUARD_TEST_LOG="$LOG" AOSC_EXEC_GUARD_DEBUG=1 \
  HOME="$PWD/$TMP/home" AOSC_EXEC_GUARD_BINFMT_DIR="$PWD/$TMP/binfmt-empty" \
  PATH="$TMP/bin:$PATH" "$GUARD" "$TMP/aarch64.elf" </dev/null > "$TMP/guard.out" 2> "$TMP/guard.err"
code=$?
set -e
[ "$code" -eq 126 ] || fail "exit code should be 126, got $code"
[ -s "$LOG" ] || fail 'stub zenity was never called'
grep -q 'aarch64' "$LOG" || fail 'dialog text should mention aarch64'
grep -q 'mode=Dialog' "$TMP/guard.err" || fail 'decision should be Dialog in this setup'
echo 'stub zenity received:'
sed 's/^/  /' "$LOG"

step 'qemu: never → explain only, no emulator'
set +e
out=$(AOSC_EXEC_GUARD_QEMU=never run_guard_qemu "$TMP/aarch64.elf" 2>&1)
code=$?
set -e
printf '%s\nexit=%s\n' "$out" "$code"
[ "$code" -eq 126 ] || fail "exit code should be 126, got $code"
case "$out" in *'qemu-aarch64'*) ;; *) fail 'message should mention the installed qemu-aarch64' ;; esac
case "$out" in *'stub-qemu'*) fail 'qemu must not run in never mode' ;; esac

step 'qemu: always → hand the program (and its arguments) to the emulator'
set +e
out=$(AOSC_EXEC_GUARD_QEMU=always run_guard_qemu "$TMP/aarch64.elf" one "two words" --flag 2>&1)
code=$?
set -e
printf '%s\nexit=%s\n' "$out" "$code"
[ "$code" -eq 42 ] || fail "exit status should come from the stub qemu (42), got $code"
case "$out" in *'stub-qemu'*) ;; *) fail 'stub qemu should have been used' ;; esac
case "$out" in *'aarch64.elf one two words --flag'*) ;; *) fail 'program arguments should be forwarded verbatim' ;; esac

step 'qemu: ask without any way to ask → keep the old behavior (just run it)'
set +e
out=$(AOSC_EXEC_GUARD_QEMU=ask run_guard_qemu "$TMP/aarch64.elf" 2>&1)
code=$?
set -e
printf '%s\nexit=%s\n' "$out" "$code"
[ "$code" -eq 42 ] || fail "no tty and no GUI: the stub qemu should have run, got $code"

step 'qemu: terminal prompt (pty): y / n / a / s'
cat > "$TMP/bin/ask-guard" <<EOF
#!/usr/bin/env bash
export HOME="$PWD/$TMP/home" AOSC_EXEC_GUARD_BINFMT_DIR="$PWD/$TMP/binfmt" AOSC_EXEC_GUARD_QEMU=ask
unset DISPLAY WAYLAND_DISPLAY XDG_SESSION_TYPE INVOCATION_ID XDG_CONFIG_HOME
exec "$GUARD" "\$@"
EOF
chmod +x "$TMP/bin/ask-guard"
ask_pty() { # $1 = answer; sets $out and $code, config file is removed first
  rm -f "$PWD/$TMP/home/.config/aosc-exec-guard.conf"
  set +e
  printf '%s\n' "$1" | script -qec "$PWD/$TMP/bin/ask-guard $TMP/aarch64.elf" /dev/null \
    > "$TMP/pty.out" 2>&1
  code=$?
  set -e
  out=$(tr -d '\r' < "$TMP/pty.out")
  printf '%s\n' "$out"
  printf 'answer=%s exit=%s\n' "$1" "$code"
}
ask_pty y
[ "$code" -eq 42 ] || fail "answering y should run the stub qemu, got $code"
case "$out" in *'stub-qemu'*) ;; *) fail 'the prompt should lead to the stub qemu' ;; esac
ask_pty n
[ "$code" -eq 126 ] || fail "answering n should explain and exit 126, got $code"
case "$out" in *'无法运行'*) ;; *) fail 'declining should print the explanation' ;; esac
ask_pty a
[ "$code" -eq 42 ] || fail "answering a should run the stub qemu, got $code"
grep -q 'qemu = always' "$PWD/$TMP/home/.config/aosc-exec-guard.conf" \
  || fail 'answering a should remember “always run”'
ask_pty s
[ "$code" -eq 126 ] || fail "answering s should explain and exit 126, got $code"
grep -q 'qemu = never' "$PWD/$TMP/home/.config/aosc-exec-guard.conf" \
  || fail 'answering s should remember “never run”'

step 'qemu: GUI checkbox (stub zenity) is only remembered when ticked'
rm -f "$PWD/$TMP/home/.config/aosc-exec-guard.conf"
run_gui_ask() {
  : > "$LOG"
  set +e
  env -u INVOCATION_ID -u XDG_CONFIG_HOME DISPLAY=:99 AOSC_GUARD_TEST_LOG="$LOG" \
    HOME="$PWD/$TMP/home" AOSC_EXEC_GUARD_BINFMT_DIR="$PWD/$TMP/binfmt" AOSC_EXEC_GUARD_QEMU=ask \
    PATH="$TMP/bin:$PATH" "$@" "$GUARD" "$TMP/aarch64.elf" </dev/null > "$TMP/g.out" 2> "$TMP/g.err"
  code=$?
  set -e
  printf 'exit=%s\n' "$code"
}
run_gui_ask env  # 没勾选框：本次运行，不记住
[ "$code" -eq 42 ] || fail "the dialog should have run the stub qemu, got $code"
grep -q -- '--checklist' "$LOG" || fail 'zenity should get the checklist dialog'
[ ! -e "$PWD/$TMP/home/.config/aosc-exec-guard.conf" ] || fail 'unticked box must not be remembered'
run_gui_ask env AOSC_GUARD_TEST_ZENITY_CHECKED=1
[ "$code" -eq 42 ] || fail "the dialog should have run the stub qemu, got $code"
grep -q 'qemu = always' "$PWD/$TMP/home/.config/aosc-exec-guard.conf" \
  || fail 'ticked box should remember “always run”'
rm -f "$PWD/$TMP/home/.config/aosc-exec-guard.conf"
run_gui_ask env AOSC_GUARD_TEST_ZENITY_CANCEL=1
[ "$code" -eq 126 ] || fail "cancelling should explain and exit 126, got $code"
grep -q -- '--error' "$LOG" || fail 'declining should end in the explanation dialog'

step 'qemu: saved answer wins, --qemu overrides it'
printf 'qemu = always\n' > "$PWD/$TMP/home/.config/aosc-exec-guard.conf"
set +e
out=$(run_guard_qemu "$TMP/aarch64.elf" 2>&1)
code=$?
set -e
printf 'config=always exit=%s\n' "$code"
[ "$code" -eq 42 ] || fail "a saved “always” should run the stub qemu, got $code"
set +e
out=$(run_guard_qemu --qemu=never "$TMP/aarch64.elf" 2>&1)
code=$?
set -e
printf 'config=always + --qemu=never exit=%s\n' "$code"
[ "$code" -eq 126 ] || fail "--qemu=never should override the saved answer, got $code"
rm -f "$PWD/$TMP/home/.config/aosc-exec-guard.conf"

if [ -x "$TMP/busybox-aarch64" ]; then
  step 'direct: real aarch64 binary (Alpine busybox-static) → explain'
  set +e
  out=$(run_guard "$TMP/busybox-aarch64" 2>&1)
  code=$?
  set -e
  printf '%s\nexit=%s\n' "$out" "$code"
  [ "$code" -eq 126 ] || fail "exit code should be 126, got $code"
  case "$out" in *aarch64*) ;; *) fail 'message should mention aarch64' ;; esac
else
  step 'real aarch64 binary not present (optional)'
  echo 'run scripts/get-test-binary.sh to fetch busybox-static for aarch64'
fi

step 'local checks passed'
echo 'next: scripts/test.sh && sudo scripts/kernel-test.sh'
